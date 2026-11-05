// pi-durable:internal/harness-service — the Harness, Session binding, task runtimes and every
// host callback of the in-process durable service (src/dispatch/transport.rs). Port of
// scripts/stock/durable-task-adapter.mjs onto the in-process transport: the native service owns
// task state, scheduling, invocation lifetime and the Session line; this module only runs the JS
// code it is given (task phases, tools, hooks, sections, storages, listeners, model collections).
// The built-in generation, tool and compaction tasks always run natively.
import { describeTelemetry } from "pi-durable:internal/telemetry";
import { installSessionBinding, getDefaultDocumentRetention } from "pi-durable:internal/src/durable/session/session";
import { transactionProbes } from "pi-durable:internal/src/durable/session/transaction";
import { nativeMemoryHandle } from "pi-durable:internal/memory-storage";
import { nativeStorageHandle } from "pi-durable:internal/native-storage";
import { environmentContext } from "pi-durable:internal/env-signals";
import { nativeTimer } from "pi-durable:internal/src/durable/timers";
import { mergeClientIdentity, validateToolArguments, withClientIdentity } from "@earendil-works/pi-ai";
import { ConversationBusy, ReadOnlySession, ReadAfterWrite, call as hostCall } from "pi-durable:bridge";
import { BACKGROUND_CONTEXT, withAbortSignal, withoutAbortSignal } from "@earendil-works/chord/context";
import { copyJson } from "@earendil-works/chord";
import { track } from "@earendil-works/chord/delta";
import { nativeTransport, failure as error } from "pi-durable:internal/transport";
import { handle, describeTask, describeDoc, objectFor, valueHandle, temporaryHandle } from "pi-durable:internal/handles";
import { settingsRequest, hydrateSettings } from "pi-durable:internal/settings";
import { nativeRegistryId, lookupNativeSnapshot, snapshot, hydrateAgent } from "pi-durable:internal/registry";

/**
 * Without `options.now` the Harness reads `Date.now`. While `Date.now` is the builtin, the native
 * clock reads the same system time without a callback per read; a replaced `Date.now` (fake
 * timers) is called through. Decided when the Harness opens.
 */
const builtinDateNow = Date.now;

/** `harness/harness.ts` `DEFAULT_HARNESS_IDENTITY`: pi's attribution. */
export const DEFAULT_HARNESS_IDENTITY = hostCall("harness.defaultIdentity");

nativeTransport.register("docInitial", ({ id, seed }) => copyJson(objectFor(id).definition.initial(seed)));
nativeTransport.register("docMigrate", ([id, value, version]) => copyJson(objectFor(id).definition.migrate(value, version)));
nativeTransport.register("docCheckpoint", ({ id, value, ops, info }) => objectFor(id).definition.checkpointWhen(graphValue(value), graphValue(ops), info));
nativeTransport.error("conversationBusy", (value) => new ConversationBusy(value.conversationId));
nativeTransport.error("readOnly", (value) => new ReadOnlySession(value.operation));
nativeTransport.error("readAfterWrite", (value) => new ReadAfterWrite(value.method));
let storageNext = 1, callbackNext = 1;
const storages = new Map(), callbacks = new Map(), runtimes = new Map(), harnesses = new Map(), graphRevisions = new Map();
const drafts = new Map(), viewBindings = new WeakMap();
const crashOwners = new WeakMap();
function trackCrashFixture(storage, owner) {
  if (typeof storage.crash !== "function") return;
  let owners = crashOwners.get(storage);
  if (owners === undefined) {
    owners = new Set();
    crashOwners.set(storage, owners);
    const crash = storage.crash;
    Object.defineProperty(storage, "crash", { configurable: true, writable: true, value: function(...args) {
      const result = Reflect.apply(crash, this, args);
      for (const owner2 of owners) nativeTransport.crashActor(owner2);
      owners.clear();
      return result;
    } });
  }
  owners.add(owner);
}
const tableRecords = new Map(), tableWrites = new Map();
function tableRecord(type, value) {
  if (value === undefined) return value;
  const key = type + ":" + value.id + ":" + JSON.stringify(value);
  if (!tableRecords.has(key)) tableRecords.set(key, value);
  return tableRecords.get(key);
}
function tableWrite(write) {
  if (!["conversation", "entry", "task", "submission"].includes(write.type)) return write;
  const value = tableRecord(write.type, write.value), key = write.type + ":" + value.id + ":" + JSON.stringify(value);
  if (!tableWrites.has(key)) tableWrites.set(key, { ...write, value });
  return tableWrites.get(key);
}
const sharedNodes = new Map();
function sharedValue(node) {
  if ("scalar" in node) return node.scalar;
  if (sharedNodes.has(node.id)) return sharedNodes.get(node.id);
  const value = "array" in node ? node.array.map(sharedValue) : Object.fromEntries(node.object.map(([key, child]) => [key, sharedValue(child)]));
  sharedNodes.set(node.id, value);
  return value;
}
function graphValue(snapshot2) {
  if ("shared" in snapshot2) return sharedValue(snapshot2.shared);
  if (!graphRevisions.has(snapshot2.revision)) graphRevisions.set(snapshot2.revision, snapshot2.value);
  return graphRevisions.get(snapshot2.revision);
}
function watchValue(snapshot2) {
  return graphValue(snapshot2);
}
nativeTransport.register("graphFrame", (args, context) => objectFor(args.callback)(watchValue(args.snapshot), graphValue(args.ops), withoutAbortSignal(context ?? BACKGROUND_CONTEXT)));
nativeTransport.afterCallback("graphFrame", (frame) => nativeTransport.sync("graph.turn", { watch: frame.args.watch, serial: frame.args.serial }));
nativeTransport.afterCallback("phase", (frame) => nativeTransport.sync("phase.turn", { runtime: frame.args[3], serial: frame.args[4], harness: frame.args[5] }));
nativeTransport.afterCallback("change", (frame) => nativeTransport.sync("change.turn", { request: frame.args.request }));
nativeTransport.afterCallback("onWake", (frame) => nativeTransport.sync("host.turn", { harness: frame.args.harness }));
function graphWatch(descriptor, context, source) {
  if (descriptor === undefined) return undefined;
  const watch = descriptor.watch, closed = call("graph.closed", { watch }).then((end) => end.error ? { ...end, error: error(end.error) } : end);
  closed.catch(() => {
  });
  let stopped, listener;
  const held = { source }, signal = context?.abortSignal, cancel = () => nativeTransport.sync("graph.cancel", { watch });
  signal?.addEventListener("abort", cancel, { once: true });
  const release = () => {
    held.source = undefined;
    listener?.release();
    signal?.removeEventListener("abort", cancel);
  };
  closed.then(release, release);
  if (signal?.aborted) cancel();
  return { get value() {
    return watchValue(nativeTransport.sync("graph.value", { watch }));
  }, closed, start: (callback) => {
    const next = temporaryHandle(callback);
    try {
      nativeTransport.sync("graph.start", { watch, callback: next.id });
      listener = next;
    } catch (error2) {
      next.release();
      throw error2;
    }
  }, stop: () => {
    if (stopped === undefined) {
      nativeTransport.sync("graph.stop", { watch });
      stopped = closed;
    }
    return stopped;
  }, cancel };
}
function docArguments(token, args) {
  const definition = describeDoc(token);
  let index = 0;
  const scope = definition.scope === "session" ? { kind: "session" } : definition.scope === "task" ? { kind: "task", taskId: args[index++] } : { kind: "conversation", conversationId: args[index++] };
  const address = { kind: definition.kind, scope };
  if (definition.family) address.key = args[index++];
  return { definition, address, index };
}
function documents(owner, source) {
  return {
    snapshot: (token, ...args) => {
      const { index, ...document } = docArguments(token, args);
      return call("doc.snapshot", { ...owner, ...document }, args[index]).then((value) => value === undefined ? undefined : graphValue(value));
    },
    snapshotAsOf: (token, ...args) => {
      const { index, ...document } = docArguments(token, args);
      return call("doc.snapshotAsOf", { ...owner, ...document, at: args[index] }, args[index + 1]).then((value) => value === undefined ? undefined : graphValue(value));
    },
    watchDoc: (token, ...args) => {
      const { index, ...document } = docArguments(token, args);
      return call("doc.watch", { ...owner, ...document }, args[index]).then((value) => graphWatch(value, args[index], source));
    }
  };
}
/** Freeze a JSON value and everything in it. */
function freezeJson(value) {
  if (value === null || typeof value !== "object" || Object.isFrozen(value)) return value;
  for (const child of Object.values(value)) freezeJson(child);
  return Object.freeze(value);
}

/**
 * Context values the service sends once and then by reference (support/context_bridge.rs
 * `ContextRefs`): entries per Harness by id, messages of an entry's `model` as that entry's own
 * message objects, others by native identity. One frozen object each, shared by every view that
 * contains it, like upstream's kept entries.
 */
const keptValues = new Map();
const keptByHarness = new Map();

function keptValue(encoded, harness) {
  if (Object.hasOwn(encoded, "$ref")) return keptValues.get(encoded.$ref);
  if (Object.hasOwn(encoded, "$m")) return keptValues.get(encoded.$m[0]).model[encoded.$m[1]];
  const value = freezeJson(encoded.v);
  keptValues.set(encoded.$def, value);
  let ids = keptByHarness.get(harness);
  if (ids === undefined) keptByHarness.set(harness, (ids = []));
  ids.push(encoded.$def);
  return value;
}

/** Forget the values kept for a closed Harness, here and in the service. */
function forgetKept(harness) {
  for (const id of keptByHarness.get(harness) ?? []) keptValues.delete(id);
  keptByHarness.delete(harness);
  call("contextRefs.forget", { namespace: harness }).catch(() => {});
}

/** A context view as upstream returns it: kept values frozen and shared, in arrays of its own. */
function contextView(encoded, harness) {
  if (encoded === undefined || encoded === null) return encoded;
  const { refs, dropped } = encoded;
  const value = (item) => keptValue(item, harness);
  const view = {};
  if (refs.head !== undefined) view.head = value(refs.head);
  view.entries = refs.entries.map(value);
  view.contributions = refs.contributions.map((messages) => Object.freeze(messages.map(value)));
  view.messages = refs.messages.map(value);
  for (const id of dropped) keptValues.delete(id);
  return view;
}

function inspect(value) {
  for (const entry of value.tasks) if (entry.state.error) entry.state.error = error(entry.state.error);
  return value;
}
const call = (method, args = {}, context) => nativeTransport.request(method, args, context);
const send = (frame) => nativeTransport.notify(frame);
nativeTransport.event("signal", (frame) => runtime(frame.runtime).controller.abort(new Error(frame.message)));
nativeTransport.event("report", (frame) => {
  try {
    Promise.resolve(harnesses.get(frame.harness)?.onReport?.(error(frame.error), frame.info)).catch(() => {
    });
  } catch {
  }
});
nativeTransport.register("timerFire", (id) => objectFor(id)());
nativeTransport.register("setTimer", ({ harness, timer, delay, background }) => {
  // Without a host `setTimer`, timers are armed with `setTimeout` like upstream's default.
  const arm = harnesses.get(harness)?.options.setTimer ?? sessions.get(harness)?.setTimer ?? nativeTimer;
  const disarm = arm(() => nativeTransport.sync("hostTimerFire", { timer }), delay, { background });
  return disarm === undefined ? null : handle(disarm);
});
nativeTransport.register("disarmTimer", (id) => {
  objectFor(id)();
  return null;
});
nativeTransport.register("tool.prepare", ({ tool, arguments: args }) => objectFor(tool).prepareArguments === undefined ? args : objectFor(tool).prepareArguments(args));
nativeTransport.register("tool.validate", ({ tool, call: call2, arguments: args }) => validateToolArguments(objectFor(tool), { ...call2, arguments: args }));
nativeTransport.register("tool.execute", async (args, context) => {
  const api = toolApi(args);
  try {
    return packToolResult(await objectFor(args.tool).execute(args.arguments, api, withAbortSignal(runtime(args.runtime).signal, context ?? BACKGROUND_CONTEXT)));
  } finally {
    toolApiStates.get(api).ended = true;
  }
});
nativeTransport.register("onWake", ({ harness, wake }) => {
  harnesses.get(harness).options.onWake(wake);
  return null;
});
nativeTransport.register("documentRetention", (id) => {
  const input = harnesses.get(id)?.options.settings, settings = typeof input === "function" ? input() : input, value = settings?.documentRetentionMs;
  return typeof value !== "number" ? null : Number.isFinite(value) ? value : String(value);
});
nativeTransport.register("conversationCreated", (args) => captureChange(args.tx, (tx) => harnesses.get(args.harness).options.conversationCreated(tx, args.record)));
nativeTransport.register("now", (id) => harnesses.get(id)?.options.now?.() ?? Date.now());
nativeTransport.register("settings", (id) => {
  const input = harnesses.get(id)?.options.settings;
  return settingsRequest(typeof input === "function" ? input() : input);
});
nativeTransport.register("readerSnapshot", (id) => {
  const value = objectFor(id).snapshot();
  return lookupNativeSnapshot(value) ?? { tasks: value.tasks().map(describeTask) };
});
nativeTransport.register("readerSubscribe", ([id, listener]) => handle(objectFor(id).subscribe(() => nativeTransport.sync("registry.notifyReader", { listener }))));
nativeTransport.register("readerUnsubscribe", (id) => objectFor(id)());
nativeTransport.register("hookMember", ({ handlers, name }) => {
  const owner = objectFor(handlers), member = owner[name];
  return typeof member === "function" ? handle(member.bind(owner)) : null;
});
nativeTransport.register("hookInvoke", ({ callback, handler }) => objectFor(callback)(objectFor(handler)));
for (const method of ["phase", "change", "storage"]) nativeTransport.register(method, (args, context, frame) => invoke(method, args, frame.abortable ? context ?? BACKGROUND_CONTEXT : withoutAbortSignal(context ?? BACKGROUND_CONTEXT)));
function change(method, args, callback, context) {
  const id = callbackNext++;
  callbacks.set(id, callback);
  const request = nativeTransport.nextId;
  const operation = call(method, { ...args, callback: id }, context).finally(() => callbacks.delete(id));
  operation.catch(() => {
  });
  if (!nativeTransport.inSynchronousCallback) nativeTransport.sync("change.turn", { request });
  return operation;
}
function configuration(change2) {
  const result = { ...change2 };
  for (const field of ["extensions", "tools", "modelTools"]) {
    const value = change2[field];
    const names = (items) => items.map((item) => typeof item === "object" && item !== null ? { name: item.name } : item);
    if (Array.isArray(value)) result[field] = names(value);
    else if (value && typeof value === "object") result[field] = Object.fromEntries(Object.entries(value).map(([key, items]) => [key, Array.isArray(items) ? names(items) : items]));
  }
  return result;
}
function transaction(id) {
  const tx = {
    conversation: (conversation) => call("tx.conversation", { tx: id, conversation }),
    scanConversations: (query, limit, cursor) => call("scanConversations", { tx: id, query, limit, cursor }),
    task: (task) => call("tx.task", { tx: id, task }),
    setTask: (record) => nativeTransport.sync("tx.setTask", { tx: id, record }),
    doc: async (token, ...args) => {
      const { index, ...document } = docArguments(token, args);
      const acquired = await call("doc.acquire", { tx: id, ...document, seed: token.definition.family ? args[index] : undefined });
      let local = drafts.get(id);
      if (!local) {
        local = new Map();
        drafts.set(id, local);
      }
      let held = local.get(acquired.draft);
      if (!held) {
        const tracker = track(acquired.value);
        held = { tracker, change: tracker.beginChange() };
        local.set(acquired.draft, held);
      }
      return held.change.state;
    },
    retireDoc: (token, ...args) => {
      const { index, ...document } = docArguments(token, args);
      return call("doc.retire", { tx: id, ...document });
    },
    latestHeadMarker: (conversation) => call("tx.latestHeadMarker", { tx: id, conversation }),
    createConversation: (options) => call("tx.createConversation", { tx: id, options }).then((value) => tableRecord("conversation", value)),
    forkConversation: (conversation, at, options) => call("tx.forkConversation", { tx: id, conversation, at, options }),
    createTask: (task, input, options) => call("createTask", { tx: id, definition: describeTask(task), input, options }),
    appendEntry: async (first, second, third) => {
      const entry = copyJson(typeof first === "number" ? second : { ...third, kind: first.kind }, { omitUndefinedProperties: true });
      return tableRecord("entry", await call("appendEntry", { tx: id, conversation: typeof first === "number" ? first : second, entry }));
    },
    entry: (first, second) => typeof first === "number" ? call("entry", { tx: id, entry: first }) : call("entry", { tx: id, kind: first.kind, entry: second }),
    createSubmission: (record) => call("createSubmission", { tx: id, record }),
    placeSubmission: (submission, entry) => call("placeSubmission", { tx: id, submission, entry }),
    settleSubmission: (submission, settlement) => call("settleSubmission", { tx: id, submission, settlement }),
    scanEntries: (query, limit, cursor) => call("scanEntries", { tx: id, query, limit, cursor }),
    scanTasks: (query, limit, cursor) => call("scanTasks", { tx: id, query, limit, cursor })
  };
  for (const name of Object.keys(tx)) if (name !== "setTask") {
    const invoke2 = tx[name];
    tx[name] = (...args) => {
      let result;
      try {
        result = Promise.resolve(invoke2(...args));
      } catch (error2) {
        result = Promise.reject(error2);
      }
      result.catch(() => {
      });
      return result;
    };
  }
  return tx;
}
function boundSubmission({ id, handle: handle2 }) {
  return { id, status: (context) => call("bound.submission.status", { handle: handle2 }, context), wait: (context) => call("bound.submission.wait", { handle: handle2 }, context), abort: (context) => call("bound.submission.abort", { handle: handle2 }, context) };
}
function boundConversation({ id, handle: handle2 }) {
  return { id, submit: (draft, context) => call("bound.submit", { handle: handle2, draft }, context).then(boundSubmission), abort: (context, options) => call("bound.abort", { handle: handle2, background: options?.background }, context), waitForIdle: (context) => call("bound.waitForIdle", { handle: handle2 }, context) };
}
function runtime(id) {
  let held = runtimes.get(id);
  if (held) return held;
  const controller = new AbortController();
  held = {
    controller,
    signal: controller.signal,
    get settings() {
      return hydrateSettings(nativeTransport.sync("runtimeSettings", { runtime: id }));
    },
    get models() {
      return harnesses.get(held.harness)?.currentModels();
    },
    env: (context) => call("runtimeEnv", { runtime: id }, context).then((id2) => objectFor(id2)),
    setTimer: (callback, delay) => {
      const timer = nativeTransport.sync("runtimeTimer", { runtime: id, callback: handle(callback), delay });
      return () => nativeTransport.sync("runtimeDisarmTimer", { timer });
    },
    entry: (token, entry, context) => call("runtimeEntry", { runtime: id, kind: token.kind, entry }, context),
    agent: (context) => call("runtimeAgent", { runtime: id }, context).then(hydrateAgent),
    hooks: { each: (name, invoke2) => call("runtimeHooks", { runtime: id, name, callback: handle(invoke2) }) },
    get registry() {
      return snapshot(nativeTransport.sync("runtimeRegistry", { runtime: id }));
    },
    commit: (callback, context) => change("commit", { runtime: id }, callback, context),
    memo: (name, ...args) => args.length === 1 ? call("memo", { runtime: id, name }, args[0]) : call("memo", { runtime: id, name, candidate: args[0] }, args[1]),
    getTask: (task, context) => call("runtimeGetTask", { runtime: id, task }, context),
    ownedTasks: (context) => call("runtimeOwnedTasks", { runtime: id }, context),
    waitForTask: (task, context) => call("runtimeWaitForTask", { runtime: id, task }, context),
    outcomes: (tasks, context) => call("outcomes", { runtime: id, tasks }, context),
    ...documents({ runtime: id }),
    context: (conversation, context, options) => call("runtimeContext", { runtime: id, harness: held.harness, conversation, at: options?.at, refs: true }, context).then((view) => contextView(view, held.harness)),
    conversation: (conversation, context) => call("runtimeConversation", { runtime: id, conversation }, context).then((descriptor) => descriptor === undefined ? undefined : boundConversation(descriptor)),
    now: () => nativeTransport.sync("runtimeNow", { runtime: id }),
    report: (failure, scope) => nativeTransport.sync("runtimeReport", { runtime: id, error: nativeTransport.encodeError(failure), scope }),
    sleepUntil: (until, checkpoint, context) => call("sleepUntil", { runtime: id, until: typeof until === "number" && !Number.isFinite(until) ? String(until) : until, checkpoint }, context)
  };
  runtimes.set(id, held);
  return held;
}
async function invoke(method, args, context) {
  if (method === "phase") {
    const [id, phase, task, runtimeId, _serial, harnessId] = args;
    nativeTransport.actorRuntime(runtimeId, harnessId);
    const current = runtime(runtimeId);
    current.taskId = task.id;
    current.conversationId = task.conversationId;
    current.harness = harnessId;
    const callContext = withAbortSignal(current.signal, context ?? BACKGROUND_CONTEXT);
    const definition = objectFor(id).definition;
    if (current.signal.aborted || phase === "$abort" && !nativeTransport.sync("phase.enter", { runtime: runtimeId, harness: harnessId })) return;
    return (phase === "$abort" ? definition.abort : definition.phases[phase])(task, current, callContext);
  }
  if (method === "change") return captureChange(args.tx, (tx) => callbacks.get(args.callback)(tx, args.current ?? undefined));
  if (method === "storage") {
    const storage = storages.get(args.handle);
    let values = args.args;
    if (args.method === "commit") {
      const [writes, adoption, identities] = values;
      for (const identity of identities ?? []) {
        const content = writes[identity.index].content;
        if (identity.value !== undefined) content.value = graphValue(identity.value);
        if (identity.ops !== undefined) content.ops = graphValue(identity.ops);
      }
      return storage.commit(writes.map(tableWrite), (seq) => nativeTransport.sync("storage.adopt", { adoption, seq }), context);
    }
    if (args.method.startsWith("scan") || args.method === "findLatestHeadMarker") values = values.map((value) => value === null ? undefined : value);
    return storage[args.method](...values, context);
  }
  throw new Error(`Unknown native callback ${method}`);
}
async function captureChange(tx, callback) {
  try {
    const result = await callback(transaction(tx));
    for (const [draft, held] of drafts.get(tx) ?? []) {
      const prepared = held.change.prepare();
      await call("doc.apply", { draft, ops: prepared.ops });
    }
    return result;
  } finally {
    if (drafts.has(tx)) {
      for (const held of drafts.get(tx).values()) held.change.abort();
      drafts.delete(tx);
      nativeTransport.sync("doc.release", { tx });
    }
  }
}
const Harness = { async open(storage, options, context) {
  const storageId = storageNext++;
  storages.set(storageId, storage);
  const registry = nativeRegistryId(options.registry);
  const reader = registry === undefined ? handle(options.registry) : undefined;
  const expected = nativeTransport.nextId;
  harnesses.set(expected, { options, onReport: options.onReport, storage });
  const id = await call("open", { rootTelemetry: describeTelemetry(options.telemetry), storage: storageId, nativeMemory: nativeMemoryHandle(storage), nativeSqlite: nativeStorageHandle("sqlite", storage), readOnly: options.readOnly === true, onWake: options.onWake !== undefined, setTimer: true, conversationCreated: options.conversationCreated !== undefined, nativeGeneration: true, registry, reader, environment: options.env !== undefined, clock: options.now !== undefined || Date.now !== builtinDateNow, settings: options.settings !== undefined, retention: getDefaultDocumentRetention() }, context);
  let closed;
  const observeClosed = () => closed ??= call("closed", { harness: id }).then((end) => end.error ? { ...end, error: error(end.error) } : end);
  const modelsOption = options.models;
  const identity = mergeClientIdentity(DEFAULT_HARNESS_IDENTITY, options.identity);
  const fixedModels = modelsOption && typeof modelsOption !== "function" ? withClientIdentity(modelsOption, identity) : undefined;
  const harness = {
    observeCommits: (callback) => {
      const subscription = nativeTransport.sync("view.observeCommits", { harness: id, callback: handle(callback) });
      return () => nativeTransport.sync("view.unsubscribeCommits", { subscription });
    },
    subscribeClose: (callback) => {
      const subscription = nativeTransport.sync("view.subscribeClose", { harness: id, callback: handle(callback) });
      return () => nativeTransport.sync("view.unsubscribeCommits", { subscription });
    },
    subscribeCommits: (callback) => {
      const subscription = nativeTransport.sync("view.subscribeCommits", { harness: id, callback: handle(callback) });
      return () => nativeTransport.sync("view.unsubscribeCommits", { subscription });
    },
    options,
    onReport: options.onReport,
    storage,
    currentModels: () => fixedModels ?? (typeof modelsOption === "function" ? withClientIdentity(modelsOption(), identity) : undefined),
    submission: (submission, context2) => call("submission.get", { harness: id, submission }, context2).then((submission2) => submission2 === undefined ? undefined : submissionHandle(submission2)),
    abortSubmission: (submission, context2, conversation2) => call("submission.abort", { harness: id, submission, conversation: conversation2 }, context2),
    wake: () => nativeTransport.sync("wake", { harness: id }),
    inspect: (context2) => call("inspect", { harness: id }, context2).then(inspect),
    watchTaskGraph: (context2) => call("graph.watch", { harness: id }, context2).then(graphWatch),
    ...documents({ harness: id }),
    conversation: async (idToFind, context2) => {
      const record = await call("conversation", { harness: id, conversation: idToFind }, context2);
      return record ? conversation(record.id) : undefined;
    },
    root: async (context2, options2) => conversation(await call("root", { harness: id, options: conversationOptions(options2) }, context2)),
    resume: () => {
      nativeTransport.sync("resume", { harness: id });
    },
    createConversation: async (options2, context2) => conversation(await call("createConversation", { harness: id, options: conversationOptions(options2) }, context2)),
    waitForIdle: (context2) => call("waitForIdle", { harness: id, conversation: null }, context2),
    getTask: (task, context2) => call("getTask", { harness: id, task }, context2),
    retryTask: (task, context2) => call("retryTask", { harness: id, task }, context2),
    usage: (context2) => call("usage", { harness: id }, context2),
    waitForTask: (task, context2) => call("waitForTask", { harness: id, task }, context2),
    abortTask: (task, context2) => call("abortTask", { harness: id, task }, context2),
    commit: (callback, context2) => change("commit", { harness: id, conversation: null }, callback, context2),
    commitWith: (callback, context2) => change("commit", { harness: id, conversation: null }, callback, context2),
    get closed() {
      return observeClosed();
    },
    close: (context2) => {
      const closing = nativeTransport.requestAdmitted("close", { harness: id }, context2);
      closing.then(() => forgetKept(id), () => {});
      return closing;
    },
    unloadDocuments: () => call("unloadDocuments", { harness: id }),
    readOnLine: (callback) => readSessionLine(id, callback),
    get documentCacheSizes() {
      return nativeTransport.sync("session.cache", { harness: id });
    },
    get [Symbol.for("pi-durable.schedulerIndexSizes")]() {
      return nativeTransport.sync("indexSizes", { harness: id });
    }
  };
  function submissionHandle(submission) {
    return { id: submission, status: (context2) => call("submission.status", { harness: id, submission }, context2), wait: (context2) => call("submission.wait", { harness: id, submission }, context2), abort: (context2) => call("submission.abort", { harness: id, submission, handle: true }, context2) };
  }
  function conversation(conversationId) {
    return { id: conversationId, compact: (instructions, context2) => call("conversation.compact", { harness: id, conversation: conversationId, instructions }, context2), context: (context2, options2) => call("conversationContext", { harness: id, conversation: conversationId, at: options2?.at, refs: true }, context2).then((view) => contextView(view, id)), fork: (at, options2, context2) => call("forkConversation", { harness: id, conversation: conversationId, at, options: conversationOptions(options2) }, context2).then(conversation), reset: async (handoff, context2) => {
      const entry = { kind: "pi.reset", head: "self" };
      if (handoff !== undefined) entry.model = [{ role: "user", content: handoff, timestamp: options.now?.() ?? Date.now() }];
      await call("submission.submit", { harness: id, conversation: conversationId, draft: { type: "write", entry } }, context2);
    }, watch: (context2) => call("view.watch", { harness: id, conversation: conversationId }, context2).then(graphWatch), abort: (context2, options2) => call("conversation.abort", { harness: id, conversation: conversationId, background: options2?.background }, context2), agent: (context2) => call("conversationAgent", { harness: id, conversation: conversationId }, context2).then(hydrateAgent), submit: (draft, context2) => call("submission.submit", { harness: id, conversation: conversationId, draft }, context2).then(submissionHandle), entries: (query, limit, cursor, context2) => call("entries", { harness: id, conversation: conversationId, query, limit, cursor }, context2), configure: (change2, context2) => call("configure", { harness: id, conversation: conversationId, change: configuration(change2) }, context2), waitForIdle: (context2) => call("waitForIdle", { harness: id, conversation: conversationId }, context2), commit: (callback, context2) => change("commit", { harness: id, conversation: conversationId }, callback, context2) };
  }
  viewBindings.set(harness, id);
  harnesses.set(id, harness);
  trackCrashFixture(storage, id);
  return harness;
} };
const storageIds = new WeakMap();
let shuttingDown = false;
const transport = { request: call, storage(storage) {
  let id = storageIds.get(storage);
  if (id === undefined) {
    id = storageNext++;
    storageIds.set(storage, id);
    storages.set(id, storage);
  }
  return id;
}, release(range) {
  if (!shuttingDown && nativeTransport.started) send({ type: "context.release", range });
} };
const toolApiStates = new WeakMap();
function invokeCodemodeGlobal(args, context) {
  const api = toolApi(args.api), global = objectFor(args.callback);
  return global.execute(args.present ? args.args : undefined, api, withAbortSignal(runtime(args.api.runtime).signal, context ?? BACKGROUND_CONTEXT));
}
function toolApi(args) {
  const state = { ended: false }, details = [];
  let cursor = 0;
  const drain = () => {
    while (cursor < details.length && details[cursor].outcome) {
      const slot = details[cursor++];
      if (slot.outcome === "ok") slot.resolve();
      else if (slot.outcome === "error") slot.reject(slot.error);
    }
  };
  const current = runtime(args.runtime);
  current.taskId = args.taskId;
  current.conversationId = args.conversationId;
  current.harness = args.harness;
  const bound = {
    ...current,
    commit: (callback, context) => change("tool.commit", { api: args.api }, callback, context),
    createTask: (task, input, options, context) => call("tool.createTask", { api: args.api, definition: describeTask(task), input, options }, context),
    callId: args.callId,
    outputWindow: args.outputWindow ?? undefined,
    env: objectFor(args.environment),
    output: (chunk, skipped) => nativeTransport.sync("tool.output", { api: args.api, ...typeof chunk === "string" ? { text: chunk } : { bytes: Buffer.from(chunk).toString("base64") }, skipped }),
    diagnostic: (diagnostic) => nativeTransport.sync("tool.diagnostic", { api: args.api, diagnostic }),
    details: (value, context) => {
      const afterEnd = state.ended;
      return new Promise((resolve, reject) => {
        const slot = { resolve, reject };
        details.push(slot);
        call("tool.details", { api: args.api, value }, context).then(() => {
          slot.outcome = "ok";
          drain();
        }, (error2) => {
          if (afterEnd || context?.abortSignal?.aborted || nativeTransport.isCancellation(error2) || nativeTransport.isAdmissionFailure(error2)) {
            slot.outcome = "rejected";
            reject(error2);
          } else {
            slot.outcome = "error";
            slot.error = error2;
          }
          drain();
        });
      });
    },
    retainedOutput: () => nativeTransport.sync("tool.retained", { api: args.api }),
    executeTool: (name, toolArgs, context, options = {}) => call("tool.nested", { api: args.api, name, arguments: toolArgs, options }, context)
  };
  Object.defineProperty(bound, "models", { get: () => current.models });
  toolApiStates.set(bound, state);
  return bound;
}
const nativeOutputArrays = new Map(), nativeOutputRefs = new WeakMap();
function packToolResult(result) {
  const plain = { ...result }, values = {};
  for (const key of ["details", "structuredOutput"]) if (result[key] !== undefined) {
    try {
      plain[key] = copyJson(result[key], { omitUndefinedProperties: true });
    } catch {
      values[key] = valueHandle(result[key]);
      delete plain[key];
    }
  }
  return { result: plain, values, contentHost: result.output && handle(result.output), contentRef: result.output && nativeOutputRefs.get(result.output) };
}
function unpackToolResult(wire) {
  const result = wire.result;
  for (const [key, id] of Object.entries(wire.values ?? {})) result[key] = objectFor(id);
  if (wire.contentHost !== undefined) result.output = objectFor(wire.contentHost);
  else if (wire.contentRef !== undefined && nativeOutputArrays.has(wire.contentRef)) result.output = nativeOutputArrays.get(wire.contentRef);
  if (result.output && wire.contentRef !== undefined) {
    nativeOutputArrays.set(wire.contentRef, result.output);
    nativeOutputRefs.set(result.output, wire.contentRef);
  }
  return result;
}
function hookRuntime(args) {
  const current = runtime(args.runtime);
  current.taskId = args.taskId;
  current.conversationId = args.conversationId;
  current.harness = args.harness;
  return current;
}
nativeTransport.register("tool.beforeHook", (args, context) => objectFor(args.handler)(args.call, hookRuntime(args), withAbortSignal(runtime(args.runtime).signal, context ?? BACKGROUND_CONTEXT)));
nativeTransport.register("tool.afterHook", async (args, context) => {
  const result = await objectFor(args.handler)(args.call, unpackToolResult(args.result), hookRuntime(args), withAbortSignal(runtime(args.runtime).signal, context ?? BACKGROUND_CONTEXT));
  return result === undefined ? undefined : packToolResult(result);
});
nativeTransport.register("environment.build", async (args, context) => {
  const harness = harnesses.get(args.harness), build = harness.options.env;
  if (build === undefined) return undefined;
  const target = { conversationId: args.conversationId, read: harness };
  if (args.cwd !== null) target.cwd = args.cwd;
  const env = await build(target, context ?? BACKGROUND_CONTEXT);
  return env === undefined ? undefined : { handle: handle(env), namespace: env.id, cwd: env.cwd };
});
nativeTransport.register("environment.call", async (args, context) => {
  const scope = environmentContext(args, context ?? BACKGROUND_CONTEXT), callbackContext = scope.context;
  try {
    const env = objectFor(args.environment), values = [...args.args];
    if (args.method === "writeTextFile") return await env.writeFile(...values, callbackContext);
    if (args.method === "writeFile" || args.method === "appendFile") values[1] = Uint8Array.from(Buffer.from(values[1], "base64"));
    if (args.method === "exec" && values[1]?.onOutput !== undefined) {
      const callback = values[1].onOutput;
      values[1].onOutput = (text, _context, info) => nativeTransport.sync("environment.output", { callback, text, info });
    }
    const result = await env[args.method](...values, callbackContext);
    if (args.method === "cleanup") return { ok: true, value: null };
    if (result.ok) {
      if (args.method === "readBinaryFile") return { ok: true, value: Buffer.from(result.value).toString("base64") };
      return result;
    }
    return { ok: false, error: { code: result.error.code, message: result.error.message, path: result.error.path, spillPath: result.error.spillPath } };
  } finally {
    scope.release();
  }
});
nativeTransport.register("tool.copyValue", (id) => copyJson(objectFor(id), { omitUndefinedProperties: true }));
const commitListenerPromises = new Map();
function listenerResult(result) {
  if (result && typeof result.then === "function") {
    const promise = Promise.resolve(result);
    promise.catch(() => {
    });
    const capability = temporaryHandle(promise);
    commitListenerPromises.set(capability.id, capability);
    return { pending: capability.id };
  }
  return {};
}
function publicationValue(publication) {
  return { ...publication, changes: publication.changes.map(({ nativeValue, nativeOps, ...change2 }) => {
    if (nativeValue !== undefined) change2.value = graphValue(nativeValue);
    if (nativeOps !== undefined) change2.ops = graphValue(nativeOps);
    return tableWrite(change2);
  }) };
}
nativeTransport.register("commit.listenerResult", (id) => {
  const capability = commitListenerPromises.get(id);
  commitListenerPromises.delete(id);
  return objectFor(id).finally(capability.release);
});
nativeTransport.register("commit.publication", (args) => listenerResult(objectFor(args.callback)(publicationValue(args.publication), BACKGROUND_CONTEXT)));
nativeTransport.register("view.create", async (args) => handle(await objectFor(args.callback)(graphValue(args.value), () => nativeTransport.sync("view.detachObserver", { attachment: args.attachment }))));
nativeTransport.register("view.publication", (args) => {
  objectFor(args.callback).publication?.(graphValue(args.before), graphValue(args.after), args.ops, publicationValue(args.publication), BACKGROUND_CONTEXT);
  return null;
});
nativeTransport.register("view.end", (args) => {
  objectFor(args.callback).end(args.failure === null ? undefined : { error: error(args.failure) });
  return null;
});
function conversationViews(harness) {
  const id = viewBindings.get(harness);
  if (id === undefined) throw new Error("Not a Harness");
  return { attach: async (conversation, create, context) => {
    const factory = temporaryHandle(create);
    try {
      const result = await call("view.attach", { harness: id, conversation, callback: factory.id }, context);
      return { observer: objectFor(result.observer), detach: () => nativeTransport.sync("view.detachObserver", { attachment: result.attachment }) };
    } finally {
      factory.release();
    }
  } };
}
async function watchEvents(harness, conversation, context) {
  const id = viewBindings.get(harness);
  if (id === undefined) throw new Error("Not a Harness");
  const descriptor = await call("view.events", { harness: id, conversation }, context), watch = graphWatch(descriptor);
  return { snapshot: descriptor.eventSnapshot, start: (listener) => watch.start((events, _ops, delivery) => listener(events, delivery)), stop: () => watch.stop(), closed: watch.closed };
}
const modelControllers = new Map();
nativeTransport.event("models.signal", ({ stream }) => modelControllers.get(stream)?.abort(new Error("Aborted")));
nativeTransport.register("models.collection", (id) => handle(harnesses.get(id).currentModels()));
nativeTransport.register("models.get", (args) => {
  const model = objectFor(args.collection).getModel(args.reference.provider, args.reference.modelId);
  return model === undefined ? null : handle(model);
});
nativeTransport.register("models.metadata", (args) => {
  const value = objectFor(args.model)[args.property];
  return Number.isFinite(value) ? value : String(value);
});
nativeTransport.register("models.imageInput", (id) => {
  const model = objectFor(id), resize = model?.inputLimits?.images?.resize, scalar = (value) => typeof value === "number" && !Number.isFinite(value) ? String(value) : value;
  return { vision: model?.input.includes("image") ?? true, maxWidth: scalar(resize?.maxWidth), maxHeight: scalar(resize?.maxHeight), maxBytes: scalar(resize?.maxBytes) };
});
nativeTransport.register("models.field", (args) => {
  const value = objectFor(args.message)[args.name];
  return value === undefined ? { present: false } : { present: true, value: copyJson(value, { omitUndefinedProperties: true }) };
});
nativeTransport.register("models.setField", (args) => {
  if (args.present) objectFor(args.message)[args.name] = args.value;
  else delete objectFor(args.message)[args.name];
  return null;
});
nativeTransport.register("models.copy", (id) => copyJson(objectFor(id), { omitUndefinedProperties: true }));
nativeTransport.register("models.hasContent", (id) => objectFor(id).content.length !== 0);
nativeTransport.register("models.toolCalls", (id) => copyJson(objectFor(id).content.filter((block) => block.type === "toolCall"), { omitUndefinedProperties: true }));
nativeTransport.register("models.hasToolCalls", (id) => objectFor(id).content.some((block) => block.type === "toolCall"));
nativeTransport.register("models.textBlocks", (id) => objectFor(id).content.flatMap((block) => block.type === "text" ? [block.text] : []));
nativeTransport.register("models.capture", (value) => handle(value));
nativeTransport.register("models.request", async (args, context) => {
  const controller = new AbortController();
  modelControllers.set(args.stream, controller);
  if (args.aborted) controller.abort(new Error("Aborted"));
  try {
    const models = objectFor(args.collection), options = { ...args.options, signal: controller.signal, telemetry: context?.telemetry }, model = objectFor(args.model);
    if (args.method === "fetchDeferred") return handle(await models.fetchDeferred(model, args.request, options));
    if (args.method === "completeSimple") return handle(await models.completeSimple(model, args.request, options));
    const stream = models.streamSimple(model, args.request, options);
    for await (const event of stream) {
      if (event.type !== "done" && event.type !== "error") nativeTransport.notify({ type: "models.event", stream: args.stream, message: handle(event.partial) });
    }
    return handle(await stream.result());
  } finally {
    modelControllers.delete(args.stream);
  }
});
nativeTransport.register("models.cancel", async (args) => {
  const controller = new AbortController();
  modelControllers.set(args.stream, controller);
  if (args.aborted) controller.abort(new Error("Aborted"));
  try {
    await objectFor(args.collection).cancelDeferred(objectFor(args.model), args.handle, { signal: controller.signal });
    return null;
  } finally {
    modelControllers.delete(args.stream);
  }
});
function generationRuntime(args) {
  const current = runtime(args.runtime);
  current.harness = args.harness;
  current.taskId = args.taskId;
  current.conversationId = args.conversationId;
  return current;
}
nativeTransport.register("generation.render", (args, context) => {
  const read = generationRuntime(args);
  return objectFor(args.section).render({ conversationId: args.conversationId, agent: hydrateAgent(args.agent), env: objectFor(args.environment), shown: Object.fromEntries(args.shown), read }, withAbortSignal(read.signal, context ?? BACKGROUND_CONTEXT));
});
function conversationOptions(options) {
  if (options === undefined) return undefined;
  return { ...options, agent: options.agent === undefined ? undefined : configuration(options.agent), init: options.init === undefined ? undefined : handle(options.init) };
}
nativeTransport.register("conversation.init", async (args) => {
  await captureChange(args.tx, (tx) => objectFor(args.callback)(tx, args.conversation));
  return null;
});
for (const method of ["beforeRequest", "afterResponse", "onYield", "afterTools"]) {
  nativeTransport.register(`generation.${method}`, (args, context) => {
    const api = generationRuntime(args), callback = objectFor(args.callback), callContext = withAbortSignal(api.signal, context ?? BACKGROUND_CONTEXT);
    if (method === "beforeRequest") return callback({ messages: args.messages }, api, callContext);
    if (method === "afterTools") return callback(args.assistant, args.results, api, callContext);
    return callback(objectFor(args.message), api, callContext);
  });
}
const compactionSources = new WeakMap();
nativeTransport.register("compactionHook", async (args, context) => {
  const api = generationRuntime(args);
  if (!compactionSources.has(api)) compactionSources.set(api, args.source);
  const result = await objectFor(args.callback)(compactionSources.get(api), api, withAbortSignal(api.signal, context ?? BACKGROUND_CONTEXT));
  return result === undefined ? undefined : "decline" in result ? { decline: true } : { summary: result.summary };
});
const sessions = new Map();
nativeTransport.register("session.readLine", async (callback) => ({ hostResult: valueHandle(await objectFor(callback)()) }));
function readSessionLine(id, callback) {
  const capability = temporaryHandle(callback);
  const operation = call("session.readLine", { harness: id, callback: capability.id }).then((result) => objectFor(result.hostResult)).finally(capability.release);
  operation.catch(() => {
  });
  return operation;
}
nativeTransport.register("session.now", (id) => sessions.get(id).now());
nativeTransport.register("session.retention", (id) => {
  const value = sessions.get(id).retention();
  return typeof value === "number" && !Number.isFinite(value) ? String(value) : value ?? null;
});
// One callback for the clock and the retention of a cache bookkeeping step (session_bridge's
// `clock_reader`); a read that throws is null, and the kernel falls back as for each alone.
nativeTransport.register("session.clock", (id) => {
  const held = sessions.get(id);
  const number = (value) => (typeof value === "number" && !Number.isFinite(value) ? String(value) : value ?? null);
  let now = null;
  let retention = null;
  try {
    now = number(held.now());
  } catch {}
  try {
    retention = number(held.retention());
  } catch {}
  return { now, retention };
});
nativeTransport.register("session.conversationCreated", (args) => captureChange(args.tx, (tx) => sessions.get(args.harness).session.deref().conversationCreated(tx, args.record)));
nativeTransport.register("session.closeListener", (id) => listenerResult(objectFor(id)()));
nativeTransport.register("session.beforeClose", (id) => sessions.get(id).session.deref().beforeClose());
installSessionBinding((session, storage, now, retention, setTimer, readOnly) => {
  const storageId = storageNext++;
  storages.set(storageId, storage);
  const expected = nativeTransport.nextId;
  sessions.set(expected, { session: new WeakRef(session), now, retention, setTimer });
  const id = nativeTransport.sync("session.open", { storage: storageId, nativeMemory: transactionProbes ? undefined : nativeMemoryHandle(storage), readOnly, hostTimers: true, transactionProbes, clock: true });
  let closed;
  return {
    ...documents({ harness: id }, session),
    commitWith: (callback, context, scope) => change("commit", { harness: id, conversation: scope?.conversationId ?? null }, async (tx) => ({ hostResult: valueHandle(await callback(tx)) }), context).then((result) => objectFor(result.hostResult)),
    subscribeCommits: (callback) => {
      const subscription = nativeTransport.sync("view.subscribeCommits", { harness: id, callback: handle(callback) });
      return () => nativeTransport.sync("view.unsubscribeCommits", { subscription });
    },
    subscribeClose: (callback) => {
      const subscription = nativeTransport.sync("view.subscribeClose", { harness: id, callback: handle(callback) });
      return () => nativeTransport.sync("view.unsubscribeCommits", { subscription });
    },
    closed: () => closed ??= call("closed", { harness: id }).then((end) => end.error ? { ...end, error: error(end.error) } : end),
    close: (context) => nativeTransport.requestAdmitted("close", { harness: id }, context),
    unloadDocuments: () => call("unloadDocuments", { harness: id }),
    readOnLine: (callback) => readSessionLine(id, callback),
    cache: () => nativeTransport.sync("session.cache", { harness: id })
  };
});
export { Harness, conversationViews, invokeCodemodeGlobal, runtime, transport, watchEvents };
