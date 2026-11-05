// pi-durable:internal/transport — the in-process `nativeTransport`.
//
// Same API and frame protocol as scripts/stock/durable-native-transport.mjs (the process
// transport of the stock gate), over the in-process service of src/dispatch/transport.rs
// instead of a worker thread and a child process. Frames cross as strict-JSON text (lone
// surrogates escaped by JSON.stringify and by the service), so no value is lossy. A synchronous
// request blocks the JS thread in `transport.wait` and dispatches the frames that arrive meanwhile,
// so nested host callbacks (including synchronous ones) finish before it returns.

import { call } from "pi-durable:bridge";

let transport;
let next = 1;
let closed = false;
let closing = false;
let transportFailure;
let exitResolve;
let exited;
let synchronousCallbacks = 0;
let contextAdapter;
const pending = new Map();
const handlers = new Map();
const events = new Map();
const completions = new Map();
const cancellations = new WeakSet();
const admissionFailures = new WeakSet();
const errors = new Map();
const foreignErrors = new Map();
let nextForeignError = 1;
const contexts = new Map();
const runtimeOwners = new Map();
const abandoned = new Set();

function captureError(value) {
  const id = nextForeignError++;
  foreignErrors.set(id, value);
  return id;
}

function encodeError(error) {
  let name = "Error";
  let message;
  try {
    if (typeof error?.name === "string") name = error.name;
  } catch {}
  try {
    message = error instanceof Error ? error.message : String(error);
  } catch {
    try {
      message = Object.prototype.toString.call(error);
    } catch {
      message = "[unformattable error]";
    }
  }
  return { foreignId: captureError(error), name, message };
}

/** The JS error for a native failure frame: the original JS error when it came from JS. */
export function failure(value) {
  if (foreignErrors.has(value.foreignId)) return foreignErrors.get(value.foreignId);
  const error = errors.has(value.kind) ? errors.get(value.kind)(value) : new Error(value.message);
  if (value.aborted) cancellations.add(error);
  if (value.name && value.name !== "Error") error.name = value.name;
  if (value.cause) Object.defineProperty(error, "cause", { value: failure(value.cause), configurable: true, writable: true });
  return error;
}

// `PI_DURABLE_NATIVE_TRACE=1` writes every frame to stderr (`PI_DURABLE_NATIVE_TRACE_DETAIL=1`
// with its content).
const tracing = globalThis.process?.env?.PI_DURABLE_NATIVE_TRACE === "1";
const traceDetail = globalThis.process?.env?.PI_DURABLE_NATIVE_TRACE_DETAIL === "1";
function trace(direction, frame) {
  if (!tracing) return;
  const row = { at: Math.round(performance.now()), direction, type: frame.type, id: frame.id, method: frame.method, sync: frame.sync, error: frame.error ? true : undefined, ...(traceDetail ? { frame } : {}) };
  globalThis.process.stderr.write(`[transport] ${JSON.stringify(row)}\n`);
}

function send(frame) {
  trace("send", frame);
  call("transport.send", transport, JSON.stringify(frame));
}

// Upstream runs a caller's continuation before native work that a later line job starts. A native
// step waits until every frame written before it has been acknowledged here, once the microtask
// queue has drained after the frame. A phase turn yields to native microtasks, so while one is
// pumped synchronously the gate is open for that runtime's task; other synchronous requests (a
// change turn waits for its commit) keep it shut. The service posts the acknowledgement as a task of
// its own, so test timer fakes and timer counts never see it.
let received = 0;
let ackScheduled = 0;
function scheduleAck() {
  if (transport === undefined || closing || closed || ackScheduled === received) return;
  ackScheduled = received;
  call("transport.ackLater", transport, received);
}

function fail(error) {
  transportFailure = error;
  for (const [id, waiter] of pending) {
    if (!waiter.sync) {
      pending.delete(id);
      waiter.detach?.();
      waiter.reject(error);
    }
  }
}

function dispatch(frame, delivered = false, held = undefined) {
  if (!delivered && frame.type !== "transport.error" && frame.type !== "transport.exit") received++;
  // A callback after a reply must follow that reply's queued continuations, including when a
  // synchronous caller is pumping the batch itself. Its context is taken now: a `context.drop`
  // later in the same batch must not erase the context the callback was sent with.
  if (!delivered && frame.type === "callback" && !frame.sync) {
    const context = contexts.get(frame.context);
    queueMicrotask(() => dispatch(frame, true, context));
    return;
  }
  trace("receive", frame);
  if (frame.type === "context.drop") {
    contexts.delete(frame.id);
    return;
  }
  if (frame.type === "admitted") {
    const waiter = pending.get(frame.id);
    if (waiter) waiter.admitted = true;
    return;
  }
  if (frame.type === "callback") {
    if (closing || closed) return;
    const handler = handlers.get(frame.method);
    const reject = (error) => {
      if (closing || closed) return;
      send({ type: "callback.result", id: frame.id, error: encodeError(error) });
      completions.get(frame.method)?.(frame);
    };
    const reply = (value) => {
      if (closing || closed) return;
      try {
        send({ type: "callback.result", id: frame.id, ...(value === undefined ? { missing: true } : { value }) });
      } catch (error) {
        reject(error);
        return;
      }
      completions.get(frame.method)?.(frame);
    };
    if (frame.sync) synchronousCallbacks++;
    try {
      if (!handler) throw new Error(`Unknown native callback ${frame.method}`);
      const context = held ?? contexts.get(frame.context);
      const value = handler(frame.args, contextAdapter?.decode(context, frame.telemetry) ?? context, frame);
      if (frame.sync) {
        if (value && typeof value.then === "function") throw new Error(`Synchronous callback returned a Promise: ${frame.method}`);
        reply(value);
      } else Promise.resolve(value).then(reply, reject);
    } catch (error) {
      reject(error);
    } finally {
      if (frame.sync) synchronousCallbacks--;
    }
    return;
  }
  if (frame.type === "transport.error") {
    fail(new Error(frame.message));
    return;
  }
  if (frame.type === "transport.exit") {
    closed = true;
    call("transport.release", transport);
    exitResolve(frame.code);
    if (frame.code !== 0 || pending.size) fail(new Error(`Native durable service exited ${frame.code}`));
    return;
  }
  if (events.has(frame.type)) {
    events.get(frame.type)(frame);
    return;
  }
  if (abandoned.delete(frame.id)) return;
  const waiter = pending.get(frame.id);
  if (!waiter) {
    fail(new Error(`Unknown native response ${frame.id}`));
    return;
  }
  if (waiter.sync) {
    waiter.frame = frame;
    return;
  }
  pending.delete(frame.id);
  waiter.detach?.();
  frame.error ? waiter.reject(failure(frame.error)) : waiter.resolve(frame.missing ? undefined : frame.value);
}

// Frames taken from the service and not dispatched yet, oldest first. A frame's dispatch may make
// a synchronous request whose wait delivers more frames; that nested delivery must first dispatch
// the frames still pending here (a synchronous callback the service waits on can be among them),
// or the service and the nested request wait on each other.
const backlog = [];

function deliver(frames) {
  for (const text of frames) backlog.push(text);
  while (backlog.length !== 0) dispatch(JSON.parse(backlog.shift()));
  scheduleAck();
}

/** Runs on the JS thread when the service queued frames. */
function pump() {
  if (transport !== undefined) deliver(call("transport.take", transport));
}

function start() {
  if (transport !== undefined) return;
  exited = new Promise((resolve) => {
    exitResolve = resolve;
  });
  transport = call("transport.open", pump);
}

/** Dispatch queued frames, waiting up to `ms` for one. */
function drain(ms) {
  deliver(call("transport.wait", transport, ms));
}

export const nativeTransport = {
  encodeError,
  // Only explicit crash fixtures retire an actor this way. Its promises stay unresolved,
  // matching a lost process; late replies from the retired actor are discarded.
  actorRuntime(runtime, owner) {
    runtimeOwners.set(runtime, owner);
  },
  crashActor(owner) {
    for (const [id, waiter] of pending) {
      if (waiter.owner === owner) {
        pending.delete(id);
        waiter.detach?.();
        contexts.delete(id);
        abandoned.add(id);
      }
    }
  },
  contextAdapter(adapter) {
    contextAdapter = adapter;
  },
  error(kind, create) {
    if (errors.has(kind)) throw new Error(`Duplicate native error ${kind}`);
    errors.set(kind, create);
  },
  isCancellation: (error) => cancellations.has(error),
  isAdmissionFailure: (error) => admissionFailures.has(error),
  get nextId() {
    return next;
  },
  get started() {
    return transport !== undefined;
  },
  get pending() {
    return pending.size;
  },
  get inSynchronousCallback() {
    return synchronousCallbacks !== 0;
  },
  afterCallback(method, handler) {
    if (completions.has(method)) throw new Error(`Duplicate native completion ${method}`);
    completions.set(method, handler);
  },
  register(method, handler) {
    if (handlers.has(method)) throw new Error(`Duplicate native handler ${method}`);
    handlers.set(method, handler);
  },
  event(type, handler) {
    if (events.has(type)) throw new Error(`Duplicate native event ${type}`);
    events.set(type, handler);
  },
  notify(frame) {
    if (transport !== undefined && !closing) send(frame);
  },
  request(method, args = {}, context) {
    start();
    const id = next++;
    if (context !== undefined) contexts.set(id, context);
    const response = new Promise((resolve, reject) => {
      const signal = context?.abortSignal;
      const abort = () => send({ type: "abort", id, message: signal.reason?.message ?? String(signal.reason) });
      pending.set(id, {
        method,
        owner: args.harness ?? runtimeOwners.get(args.runtime),
        resolve,
        reject,
        context,
        detach: () => signal?.removeEventListener("abort", abort),
      });
      try {
        send({
          id,
          method,
          ...args,
          ...(contextAdapter ? { telemetry: contextAdapter.encode(context) } : {}),
          ...(signal?.aborted ? { abortInitially: signal.reason?.message ?? String(signal.reason) } : {}),
        });
      } catch (error) {
        pending.delete(id);
        admissionFailures.add(error);
        reject(error);
        return;
      }
      if (!signal?.aborted) signal?.addEventListener("abort", abort, { once: true });
    });
    // Native close can reject several waits before the held commit's reply arrives. Observe
    // transport failures immediately; callers still receive this same rejecting promise.
    response.catch(() => {});
    return response;
  },
  requestAdmitted(method, args = {}, context) {
    const id = next;
    const response = this.request(method, { ...args, admit: true, reentrant: synchronousCallbacks !== 0 }, context);
    const waiter = pending.get(id);
    const deadline = Date.now() + 30000;
    while (pending.has(id) && !waiter.admitted) {
      drain(0);
      if (transportFailure) throw transportFailure;
      if (closed) throw new Error("Native durable transport closed");
      if (Date.now() > deadline) throw new Error(`Native durable admission timed out: ${method}`);
      if (pending.has(id) && !waiter.admitted) drain(10);
    }
    return response;
  },
  /**
   * Run a native `memory.call` now on this thread (no frames): `{ value, adopted }`, where
   * `adopted` is the sequence a commit adopted. Throws the native failure.
   */
  memoryCall(args) {
    start();
    if (closed) throw new Error("Native durable transport closed");
    const frame = JSON.parse(call("transport.memoryCall", transport, JSON.stringify(args)));
    if (frame.error) throw failure(frame.error);
    return { value: frame.missing ? undefined : frame.value, adopted: frame.adopted };
  },
  sync(method, args = {}) {
    start();
    const id = next++;
    const waiter = { sync: true, method };
    pending.set(id, waiter);
    const opens = method === "phase.turn";
    const acked = received;
    if (opens) send({ type: "host.ack", count: acked, open: true, runtime: args.runtime });
    send({ id, method, ...args });
    const deadline = Date.now() + 30000;
    try {
      while (!waiter.frame) {
        drain(0);
        if (transportFailure) throw transportFailure;
        if (closed) throw new Error("Native durable transport closed");
        if (Date.now() > deadline) throw new Error(`Native durable request timed out: ${method}`);
        if (!waiter.frame) drain(10);
      }
      if (waiter.frame.error) throw failure(waiter.frame.error);
      return waiter.frame.missing ? undefined : waiter.frame.value;
    } finally {
      pending.delete(id);
      if (opens && !closing && !closed) send({ type: "host.ack", count: acked, open: false, runtime: args.runtime });
      scheduleAck();
    }
  },
  async close() {
    if (transport === undefined) throw new Error("No native durable request was made");
    if (pending.size) throw new Error(`Native requests still pending: ${pending.size} (${[...pending].map(([id, waiter]) => `${id}:${waiter.method}`).join(", ")})`);
    closing = true;
    call("transport.close", transport);
    if ((await exited) !== 0) throw new Error("Native durable service failed");
  },
};
