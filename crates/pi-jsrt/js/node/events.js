// node:events

const kCapture = Symbol("kCapture");
const errorMonitor = Symbol("events.errorMonitor");
let defaultMaxListeners = 10;

function checkListener(listener) {
  if (typeof listener !== "function") {
    const err = new TypeError(`The "listener" argument must be of type function. Received ${listener === null ? "null" : typeof listener}`);
    err.code = "ERR_INVALID_ARG_TYPE";
    throw err;
  }
}

function EventEmitter(opts) {
  EventEmitter.init.call(this, opts);
}

EventEmitter.init = function (opts) {
  if (this._events === undefined || this._events === Object.getPrototypeOf(this)._events) {
    this._events = Object.create(null);
    this._eventsCount = 0;
  }
  this._maxListeners = this._maxListeners || undefined;
  if (opts?.captureRejections) this[kCapture] = true;
};

EventEmitter.prototype._events = undefined;
EventEmitter.prototype._eventsCount = 0;
EventEmitter.prototype._maxListeners = undefined;

Object.defineProperty(EventEmitter, "defaultMaxListeners", {
  enumerable: true,
  get: () => defaultMaxListeners,
  set: (n) => {
    defaultMaxListeners = n;
  },
});

EventEmitter.prototype.setMaxListeners = function setMaxListeners(n) {
  this._maxListeners = n;
  return this;
};

EventEmitter.prototype.getMaxListeners = function getMaxListeners() {
  return this._maxListeners === undefined ? defaultMaxListeners : this._maxListeners;
};

function ensureEvents(target) {
  if (target._events === undefined) {
    target._events = Object.create(null);
    target._eventsCount = 0;
  }
  return target._events;
}

EventEmitter.prototype.emit = function emit(type, ...args) {
  const events = ensureEvents(this);
  if (type === "error" && events[errorMonitor] !== undefined) {
    this.emit(errorMonitor, ...args);
  }
  const handler = events[type];
  if (handler === undefined) {
    if (type === "error") {
      const er = args[0];
      if (er instanceof Error) throw er;
      const err = new Error(`Unhandled error. (${internal.inspect(er)})`);
      err.code = "ERR_UNHANDLED_ERROR";
      err.context = er;
      throw err;
    }
    return false;
  }
  const listeners = typeof handler === "function" ? [handler] : handler.slice();
  for (const listener of listeners) {
    const result = listener.apply(this, args);
    if (this[kCapture] && result && typeof result.then === "function") {
      result.then(undefined, (err) => process.nextTick(() => this.emit("error", err)));
    }
  }
  return true;
};

function addListener(target, type, listener, prepend) {
  checkListener(listener);
  const events = ensureEvents(target);
  if (events.newListener !== undefined) {
    target.emit("newListener", type, listener.listener ?? listener);
  }
  const existing = events[type];
  if (existing === undefined) {
    events[type] = listener;
    target._eventsCount++;
  } else if (typeof existing === "function") {
    events[type] = prepend ? [listener, existing] : [existing, listener];
  } else if (prepend) {
    existing.unshift(listener);
  } else {
    existing.push(listener);
  }
  const m = target.getMaxListeners?.() ?? defaultMaxListeners;
  const count = typeof events[type] === "function" ? 1 : events[type].length;
  if (m > 0 && count > m && !events[type].warned && typeof events[type] !== "function") {
    events[type].warned = true;
  }
  return target;
}

EventEmitter.prototype.addListener = function (type, listener) {
  return addListener(this, type, listener, false);
};
EventEmitter.prototype.on = EventEmitter.prototype.addListener;
EventEmitter.prototype.prependListener = function (type, listener) {
  return addListener(this, type, listener, true);
};

function onceWrapper(...args) {
  if (!this.fired) {
    this.target.removeListener(this.type, this.wrapFn);
    this.fired = true;
    return this.listener.apply(this.target, args);
  }
}

function onceWrap(target, type, listener) {
  const state = { fired: false, wrapFn: undefined, target, type, listener };
  const wrapped = onceWrapper.bind(state);
  wrapped.listener = listener;
  state.wrapFn = wrapped;
  return wrapped;
}

EventEmitter.prototype.once = function once(type, listener) {
  checkListener(listener);
  this.on(type, onceWrap(this, type, listener));
  return this;
};

EventEmitter.prototype.prependOnceListener = function prependOnceListener(type, listener) {
  checkListener(listener);
  this.prependListener(type, onceWrap(this, type, listener));
  return this;
};

EventEmitter.prototype.removeListener = function removeListener(type, listener) {
  checkListener(listener);
  const events = ensureEvents(this);
  const list = events[type];
  if (list === undefined) return this;
  if (list === listener || list.listener === listener) {
    if (--this._eventsCount === 0) this._events = Object.create(null);
    else delete events[type];
    if (events.removeListener) this.emit("removeListener", type, list.listener || listener);
  } else if (typeof list !== "function") {
    let position = -1;
    for (let i = list.length - 1; i >= 0; i--) {
      if (list[i] === listener || list[i].listener === listener) {
        position = i;
        break;
      }
    }
    if (position < 0) return this;
    const removed = list[position];
    list.splice(position, 1);
    if (list.length === 1) events[type] = list[0];
    if (events.removeListener !== undefined) this.emit("removeListener", type, removed.listener || listener);
  }
  return this;
};
EventEmitter.prototype.off = EventEmitter.prototype.removeListener;

EventEmitter.prototype.removeAllListeners = function removeAllListeners(type) {
  const events = ensureEvents(this);
  // Not listening for removeListener, no need to emit.
  if (events.removeListener === undefined) {
    if (arguments.length === 0) {
      this._events = Object.create(null);
      this._eventsCount = 0;
    } else if (events[type] !== undefined) {
      if (--this._eventsCount === 0) this._events = Object.create(null);
      else delete events[type];
    }
    return this;
  }
  // Emit removeListener for all listeners on all events.
  if (arguments.length === 0) {
    for (const key of Reflect.ownKeys(events)) {
      if (key === "removeListener") continue;
      this.removeAllListeners(key);
    }
    this.removeAllListeners("removeListener");
    this._events = Object.create(null);
    this._eventsCount = 0;
    return this;
  }
  const listeners = events[type];
  if (typeof listeners === "function") {
    this.removeListener(type, listeners);
  } else if (listeners !== undefined) {
    // LIFO order
    for (let i = listeners.length - 1; i >= 0; i--) this.removeListener(type, listeners[i]);
  }
  return this;
};

EventEmitter.prototype.listeners = function listeners(type) {
  const l = ensureEvents(this)[type];
  if (l === undefined) return [];
  if (typeof l === "function") return [l.listener || l];
  return l.map((x) => x.listener || x);
};

EventEmitter.prototype.rawListeners = function rawListeners(type) {
  const l = ensureEvents(this)[type];
  if (l === undefined) return [];
  return typeof l === "function" ? [l] : l.slice();
};

EventEmitter.prototype.listenerCount = function listenerCount(type, listener) {
  const l = ensureEvents(this)[type];
  if (l === undefined) return 0;
  const arr = typeof l === "function" ? [l] : l;
  if (listener) return arr.filter((x) => x === listener || x.listener === listener).length;
  return arr.length;
};

EventEmitter.prototype.eventNames = function eventNames() {
  return this._eventsCount > 0 ? Reflect.ownKeys(this._events) : [];
};

EventEmitter.listenerCount = (emitter, type) => emitter.listenerCount(type);

function once(emitter, name, options = {}) {
  const signal = options?.signal;
  return new Promise((resolve, reject) => {
    if (signal?.aborted) {
      reject(signal.reason ?? new internal.DOMException("The operation was aborted", "AbortError"));
      return;
    }
    if (typeof emitter.addEventListener === "function" && typeof emitter.on !== "function") {
      emitter.addEventListener(name, (ev) => resolve([ev]), { once: true });
      return;
    }
    const errorListener = (err) => {
      emitter.removeListener(name, resolver);
      reject(err);
    };
    const resolver = (...args) => {
      if (name !== "error") emitter.removeListener("error", errorListener);
      resolve(args);
    };
    emitter.once(name, resolver);
    if (name !== "error") emitter.once("error", errorListener);
    signal?.addEventListener(
      "abort",
      () => {
        emitter.removeListener(name, resolver);
        emitter.removeListener("error", errorListener);
        reject(signal.reason);
      },
      { once: true },
    );
  });
}

function on(emitter, event, options = {}) {
  const queue = [];
  const waiters = [];
  let error = null;
  let done = false;
  const handler = (...args) => {
    const w = waiters.shift();
    if (w) w.resolve({ value: args, done: false });
    else queue.push(args);
  };
  const errorHandler = (err) => {
    done = true;
    const w = waiters.shift();
    if (w) w.reject(err);
    else error = err;
    iterator.return();
  };
  emitter.on(event, handler);
  if (event !== "error") emitter.on("error", errorHandler);
  const iterator = {
    next() {
      if (queue.length) return Promise.resolve({ value: queue.shift(), done: false });
      if (error) {
        const e = error;
        error = null;
        return Promise.reject(e);
      }
      if (done) return Promise.resolve({ value: undefined, done: true });
      return new Promise((resolve, reject) => waiters.push({ resolve, reject }));
    },
    return() {
      emitter.removeListener(event, handler);
      emitter.removeListener("error", errorHandler);
      done = true;
      for (const w of waiters.splice(0)) w.resolve({ value: undefined, done: true });
      return Promise.resolve({ value: undefined, done: true });
    },
    throw(err) {
      error = err;
      return iterator.return();
    },
    [Symbol.asyncIterator]() {
      return this;
    },
  };
  options?.signal?.addEventListener("abort", () => iterator.return(), { once: true });
  return iterator;
}

function getEventListeners(emitter, name) {
  if (typeof emitter.listeners === "function") return emitter.listeners(name);
  return [];
}

function getMaxListeners(emitterOrTarget) {
  if (typeof emitterOrTarget?.getMaxListeners === "function") return emitterOrTarget.getMaxListeners();
  return defaultMaxListeners;
}

// Node: listen for `abort` once and return a Disposable that removes the listener.
function addAbortListener(signal, listener) {
  if (signal === undefined) throw new TypeError('The "signal" argument must be an instance of AbortSignal. Received undefined');
  if (typeof listener !== "function") throw new TypeError('The "listener" argument must be of type function');
  let removed = false;
  const handler = () => listener.call(signal, new Event("abort"));
  if (signal.aborted) queueMicrotask(handler);
  else signal.addEventListener("abort", handler, { once: true });
  const dispose = () => {
    if (removed) return;
    removed = true;
    signal.removeEventListener("abort", handler);
  };
  return { [Symbol.dispose ?? Symbol.for("nodejs.dispose")]: dispose };
}

function setMaxListeners(n = defaultMaxListeners, ...targets) {
  if (targets.length === 0) defaultMaxListeners = n;
  for (const t of targets) t.setMaxListeners?.(n);
}

EventEmitter.EventEmitter = EventEmitter;
EventEmitter.once = once;
EventEmitter.on = on;
EventEmitter.errorMonitor = errorMonitor;
EventEmitter.captureRejectionSymbol = Symbol.for("nodejs.rejection");
EventEmitter.getEventListeners = getEventListeners;
EventEmitter.setMaxListeners = setMaxListeners;
EventEmitter.getMaxListeners = getMaxListeners;
EventEmitter.addAbortListener = addAbortListener;
EventEmitter.usingDomains = false;
EventEmitter.EventEmitterAsyncResource = EventEmitter;
Object.defineProperty(EventEmitter, "captureRejections", { value: false, writable: true, enumerable: true });

internal.EventEmitter = EventEmitter;
internal.defineBuiltin("events", () => EventEmitter);
