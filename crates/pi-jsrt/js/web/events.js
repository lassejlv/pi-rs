// DOMException, Event, EventTarget, AbortController, AbortSignal

const domCodes = {
  IndexSizeError: 1,
  HierarchyRequestError: 3,
  WrongDocumentError: 4,
  InvalidCharacterError: 5,
  NoModificationAllowedError: 7,
  NotFoundError: 8,
  NotSupportedError: 9,
  InvalidStateError: 11,
  SyntaxError: 12,
  InvalidModificationError: 13,
  NamespaceError: 14,
  InvalidAccessError: 15,
  TypeMismatchError: 17,
  SecurityError: 18,
  NetworkError: 19,
  AbortError: 20,
  URLMismatchError: 21,
  QuotaExceededError: 22,
  TimeoutError: 23,
  InvalidNodeTypeError: 24,
  DataCloneError: 25,
};

class DOMException extends Error {
  #name;
  constructor(message = "", options = "Error") {
    super(message, typeof options === "object" && options && "cause" in options ? { cause: options.cause } : undefined);
    this.#name = typeof options === "object" && options ? String(options.name ?? "Error") : String(options);
  }
  get name() {
    return this.#name;
  }
  get code() {
    return domCodes[this.#name] ?? 0;
  }
}

class Event {
  #type;
  #stop = false;
  #canceled = false;
  constructor(type, init = {}) {
    if (arguments.length === 0) throw new TypeError('The "type" argument must be specified');
    this.#type = String(type);
    this.bubbles = Boolean(init?.bubbles);
    this.cancelable = Boolean(init?.cancelable);
    this.composed = Boolean(init?.composed);
    this.timeStamp = ops.now();
    this.target = null;
    this.currentTarget = null;
    this.isTrusted = false;
    this.eventPhase = 0;
  }
  get type() {
    return this.#type;
  }
  get defaultPrevented() {
    return this.cancelable && this.#canceled;
  }
  get cancelBubble() {
    return this.#stop;
  }
  preventDefault() {
    this.#canceled = true;
  }
  stopPropagation() {}
  stopImmediatePropagation() {
    this.#stop = true;
  }
  get returnValue() {
    return !this.defaultPrevented;
  }
  composedPath() {
    return this.target ? [this.target] : [];
  }
  get [Symbol.toStringTag]() {
    return "Event";
  }
}

class CustomEvent extends Event {
  #detail;
  constructor(type, init = {}) {
    super(type, init);
    this.#detail = init?.detail ?? null;
  }
  get detail() {
    return this.#detail;
  }
}

const kListeners = Symbol("listeners");

class EventTarget {
  constructor() {
    internal.hidden(this, kListeners, new Map());
  }
  addEventListener(type, listener, options = {}) {
    if (!listener) return;
    if (typeof options === "boolean") options = { capture: options };
    const list = this[kListeners].get(type) ?? [];
    if (list.some((l) => l.listener === listener)) return;
    const entry = { listener, once: Boolean(options?.once) };
    list.push(entry);
    this[kListeners].set(type, list);
    const signal = options?.signal;
    if (signal) {
      if (signal.aborted) {
        this.removeEventListener(type, listener);
      } else {
        signal.addEventListener("abort", () => this.removeEventListener(type, listener), { once: true });
      }
    }
  }
  removeEventListener(type, listener) {
    const list = this[kListeners].get(type);
    if (!list) return;
    const idx = list.findIndex((l) => l.listener === listener);
    if (idx >= 0) list.splice(idx, 1);
  }
  dispatchEvent(event) {
    if (!(event instanceof Event)) throw new TypeError('The "event" argument must be an instance of Event');
    event.target = this;
    event.currentTarget = this;
    const handler = this[`on${event.type}`];
    const list = (this[kListeners].get(event.type) ?? []).slice();
    if (typeof handler === "function") list.unshift({ listener: handler, once: false, prop: true });
    for (const entry of list) {
      if (entry.once) this.removeEventListener(event.type, entry.listener);
      try {
        if (typeof entry.listener === "function") entry.listener.call(this, event);
        else if (entry.listener && typeof entry.listener.handleEvent === "function") entry.listener.handleEvent(event);
      } catch (err) {
        ops.reportError("Uncaught exception", err);
      }
      if (event.cancelBubble) break;
    }
    event.currentTarget = null;
    return !event.defaultPrevented;
  }
}

const kAborted = Symbol("aborted");
const kReason = Symbol("reason");

class AbortSignal extends EventTarget {
  constructor(key) {
    if (key !== kAborted) throw new TypeError("Illegal constructor");
    super();
    internal.hidden(this, kAborted, false);
    internal.hidden(this, kReason, undefined);
    this.onabort = null;
  }
  get aborted() {
    return this[kAborted];
  }
  get reason() {
    return this[kReason];
  }
  throwIfAborted() {
    if (this[kAborted]) throw this[kReason];
  }
  static abort(reason) {
    const s = new AbortSignal(kAborted);
    s[kAborted] = true;
    s[kReason] = reason === undefined ? new DOMException("This operation was aborted", "AbortError") : reason;
    return s;
  }
  static timeout(ms) {
    const s = new AbortSignal(kAborted);
    const t = setTimeout(() => internal.abortSignal(s, new DOMException("The operation was aborted due to timeout", "TimeoutError")), ms);
    t.unref?.();
    return s;
  }
  static any(signals) {
    const s = new AbortSignal(kAborted);
    for (const sig of signals) {
      if (sig.aborted) {
        internal.abortSignal(s, sig.reason);
        return s;
      }
    }
    for (const sig of signals) sig.addEventListener("abort", () => internal.abortSignal(s, sig.reason), { once: true });
    return s;
  }
  get [Symbol.toStringTag]() {
    return "AbortSignal";
  }
}

internal.abortSignal = (signal, reason) => {
  if (signal[kAborted]) return;
  signal[kAborted] = true;
  signal[kReason] = reason === undefined ? new DOMException("This operation was aborted", "AbortError") : reason;
  signal.dispatchEvent(new Event("abort"));
};

class AbortController {
  #signal = new AbortSignal(kAborted);
  get signal() {
    return this.#signal;
  }
  abort(reason) {
    internal.abortSignal(this.#signal, reason);
  }
  get [Symbol.toStringTag]() {
    return "AbortController";
  }
}

internal.Event = Event;
internal.clearEventListeners = (target, name) => (name === undefined ? target[kListeners].clear() : target[kListeners].delete(name));
internal.EventTarget = EventTarget;
internal.DOMException = DOMException;
for (const [name, value] of Object.entries({ DOMException, Event, CustomEvent, EventTarget, AbortController, AbortSignal })) {
  internal.hidden(globalThis, name, value);
}
