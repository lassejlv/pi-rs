// packages/durable/src/session/session.ts over the native Session kernel (the in-process stock
// service, src/dispatch/transport.rs). Port of scripts/stock/durable-session-adapter.mjs: the
// binding (installed by the transport adapter) owns the native Session; this class keeps the
// upstream shape so tests and Harness glue can construct and extend it.

let bind;
let defaultRetention = 600_000;
let defaultTimer;

function operation(start) {
  let result;
  try {
    result = Promise.resolve(start());
  } catch (error) {
    result = Promise.reject(error);
  }
  result.catch(() => {});
  return result;
}

/** Internal, for tests: the retention of every Session that keeps the default. */
export function setDefaultDocumentRetention(value) {
  defaultRetention = value;
}

export function getDefaultDocumentRetention() {
  return defaultRetention;
}

/** A retention as given, when it is a number of milliseconds, `0` or more; otherwise the default. */
export function documentRetention(ms) {
  return typeof ms === "number" && ms >= 0 ? ms : defaultRetention;
}

export function setDefaultSessionTimer(timer) {
  defaultTimer = timer;
}

/** Installed by the transport adapter: `(session, storage, now, retention, setTimer, readOnly) => binding`. */
export function installSessionBinding(binding) {
  bind = binding;
}

export class SessionImpl {
  constructor(storage, now = Date.now, retention = () => undefined, setTimer = defaultTimer, readOnly = false) {
    if (bind === undefined) throw new Error("Native Session transport was not installed");
    this.binding = bind(this, storage, now, () => retention() ?? defaultRetention, setTimer, readOnly);
  }

  commit(callback, context) {
    return this.commitWith(callback, context);
  }

  commitWith(callback, context, scope) {
    return operation(() => this.binding.commitWith(callback, context, scope));
  }

  snapshot(...args) {
    return operation(() => this.binding.snapshot(...args));
  }

  snapshotAsOf(...args) {
    return operation(() => this.binding.snapshotAsOf(...args));
  }

  watchDoc(...args) {
    return operation(() => this.binding.watchDoc(...args));
  }

  readOnLine(job) {
    return operation(() => this.binding.readOnLine(job));
  }

  subscribeClose(callback) {
    return this.binding.subscribeClose(callback);
  }

  subscribeCommits(callback) {
    return this.binding.subscribeCommits(callback);
  }

  get closed() {
    return this.binding.closed();
  }

  close(context) {
    return operation(() => this.binding.close(context));
  }

  unloadDocuments() {
    return operation(() => this.binding.unloadDocuments());
  }

  get documentCacheSizes() {
    return this.binding.cache();
  }

  async conversationCreated() {}

  async beforeClose() {}

  report() {}
}

/**
 * Open a Session kernel over one storage backend. `now` is the wall clock for task times; default `Date.now`.
 * `setTimer` arms the timers that evict idle documents; default `setTimeout`.
 */
export function createSession(storage, options) {
  const retention = options?.documentRetentionMs;
  return new SessionImpl(storage, options?.now, retention === undefined ? undefined : () => retention, options?.setTimer);
}

/**
 * Run a host callback that must not fail its caller: a throw, or a promise it returns that rejects, goes to `onError`,
 * which is not awaited.
 */
export function contained(callback, onError) {
  try {
    const result = callback();
    // Any thenable, also a promise from another realm; `Promise.resolve` adopts it with handlers of its own.
    if (typeof result?.then === "function") Promise.resolve(result).catch(onError);
  } catch (error) {
    onError(error);
  }
}
