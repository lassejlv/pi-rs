// pi-durable:bridge — the glue's view of the in-process host (src/host.rs) and the error classes
// native failures are reconstructed as (src/errors.rs). The classes are the upstream ones
// (packages/durable/src/errors.ts, env/index.ts, storage/jsonl/storage.ts,
// packages/chord/src/errors.ts, delta/index.ts); public modules re-export them from here so every
// module sees one class identity.

const raw = globalThis.__piDurable;
if (raw === undefined) throw new Error("pi-durable host is not installed in this runtime");

/** Call a synchronous host method. */
export const call = raw.call;
/** Call an asynchronous host method; returns a promise. */
export const callAsync = raw.callAsync;
/** Pass `value` to the host as a handle even when it is plain data. */
export const ref = raw.ref;
/** `"sync"`, `"async"` or `undefined`: whether the host has `name`. */
export const method = raw.method;

// --- @earendil-works/pi-durable (errors.ts) ---

export class ReadAfterWrite extends Error {
  constructor(method) {
    super(`Tx.${method}() cannot read tables after the first table write`);
    this.name = "ReadAfterWrite";
  }
}

export class StorageRequestError extends Error {
  constructor(message) {
    super(message);
    this.name = "StorageRequestError";
  }
}

export class SessionFailed extends Error {
  constructor(cause) {
    super("Session failed after a storage error; close and reopen it", { cause });
    this.name = "SessionFailed";
  }
}

export class ReadOnlySession extends Error {
  constructor(what) {
    super(`Read-only Harness: ${what} refused`);
    this.name = "ReadOnlySession";
  }
}

export class ConversationBusy extends Error {
  conversationId;
  constructor(conversationId) {
    super(`Conversation ${conversationId} is busy`);
    this.name = "ConversationBusy";
    this.conversationId = conversationId;
  }
}

// --- @earendil-works/pi-durable/env (env/index.ts) ---

export class FileError extends Error {
  code;
  path;
  constructor(code, message, path, cause) {
    super(message, cause === undefined ? undefined : { cause });
    this.name = "FileError";
    this.code = code;
    this.path = path;
  }
}

export class ExecutionError extends Error {
  code;
  spillPath;
  constructor(code, message, cause) {
    super(message, cause === undefined ? undefined : { cause });
    this.name = "ExecutionError";
    this.code = code;
  }
}

// --- @earendil-works/pi-durable/storage/jsonl (storage/jsonl/storage.ts) ---

export class JsonlCorruptionError extends Error {
  constructor(message, cause) {
    super(message, cause === undefined ? undefined : { cause });
    this.name = "JsonlCorruptionError";
  }
}

export class JsonlStoragePoisonedError extends Error {
  constructor(cause) {
    super("JSONL storage is poisoned and must be reopened", { cause });
    this.name = "JsonlStoragePoisonedError";
  }
}

// --- @earendil-works/chord (errors.ts) ---

const RETRYABLE = new Set(["superseded", "upgrade_conflict"]);

export class ChordError extends Error {
  code;
  reason;
  retryable;
  details;
  constructor(code, message, options) {
    super(`${code}${options?.reason ? `/${options.reason}` : ""}: ${message}`, {
      cause: options?.cause,
    });
    this.name = "ChordError";
    this.code = code;
    this.reason = options?.reason;
    this.retryable = RETRYABLE.has(code);
    this.details = options?.details;
  }
}

// --- @earendil-works/chord/delta (delta/index.ts) ---

export class UnsafePathError extends Error {
  segment;
  constructor(segment) {
    super(`unsafe path segment: ${String(segment)}`);
    this.segment = segment;
    this.name = "UnsafePathError";
  }
}

export class PathError extends Error {
  path;
  constructor(path) {
    super(`unresolvable path: ${JSON.stringify(path)}`);
    this.path = path;
    this.name = "PathError";
  }
}

// --- Native failure reconstruction (src/errors.rs table) ---

const own = (o, k) => Object.prototype.hasOwnProperty.call(o, k);

/** `name -> [factory(message, props, cause, hasCause), consumed prop names]` */
const factories = {
  Error: [(m, p, c, has) => new Error(m, has ? { cause: c } : undefined), []],
  TypeError: [(m, p, c, has) => new TypeError(m, has ? { cause: c } : undefined), []],
  RangeError: [(m, p, c, has) => new RangeError(m, has ? { cause: c } : undefined), []],
  AbortError: [(m) => new DOMException(m, "AbortError"), []],
  ConversationBusy: [(m, p) => new ConversationBusy(p.conversationId), ["conversationId"]],
  ReadAfterWrite: [(m, p) => new ReadAfterWrite(p.method), ["method"]],
  ReadOnlySession: [(m, p) => new ReadOnlySession(p.what), ["what"]],
  SessionFailed: [(m, p, c) => new SessionFailed(c), []],
  StorageRequestError: [(m) => new StorageRequestError(m), []],
  FileError: [(m, p, c, has) => new FileError(p.code, m, p.path, has ? c : undefined), ["code", "path"]],
  ExecutionError: [
    (m, p, c, has) => {
      const error = new ExecutionError(p.code, m, has ? c : undefined);
      if (own(p, "spillPath")) error.spillPath = p.spillPath;
      return error;
    },
    ["code", "spillPath"],
  ],
  JsonlCorruptionError: [(m, p, c, has) => new JsonlCorruptionError(m, has ? c : undefined), []],
  JsonlStoragePoisonedError: [(m, p, c) => new JsonlStoragePoisonedError(c), []],
  ChordError: [
    (m, p, c, has) => new ChordError(p.code, m, { ...(has ? { cause: c } : {}), details: p.details, reason: p.reason }),
    ["code", "details", "reason"],
  ],
  PathError: [(m, p) => new PathError(p.path), ["path"]],
  UnsafePathError: [(m, p) => new UnsafePathError(p.segment), ["segment"]],
  WatchError: [
    (m, p, c, has) => {
      const error = new Error(m, has ? { cause: c } : undefined);
      error.name = "WatchError";
      return error;
    },
    [],
  ],
};

/** Error class names the bridge reconstructs (must match `ErrorClass::ALL`). */
export const errorClassNames = Object.freeze(Object.keys(factories));

/** Build the error a native failure throws. Called by the host. */
function makeError(name, message, props, cause, hasCause) {
  const entry = own(factories, name) ? factories[name] : factories.Error;
  const error = entry[0](message, props, cause, hasCause);
  for (const key of Object.keys(props)) if (!entry[1].includes(key) && !own(error, key)) error[key] = props[key];
  return error;
}

Object.defineProperty(raw, "makeError", { value: makeError, configurable: true });
