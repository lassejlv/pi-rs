// node:diagnostics_channel — named channels with publish/subscribe, store binding and tracing
// channels, following lib/diagnostics_channel.js. pi-jsrt's net and http publish on Node's
// built-in channel names (net.client.socket, http.client.request.start, ...).

function codeError(Ctor, code, message) {
  const err = new Ctor(message);
  err.code = code;
  return err;
}

function validateFunction(fn, name) {
  if (typeof fn !== "function") throw codeError(TypeError, "ERR_INVALID_ARG_TYPE", `The "${name}" argument must be of type function. Received ${internal.describeReceived(fn)}`);
}

// Channels are kept while they have subscribers or bound stores; otherwise only weakly.
const channels = new Map(); // name -> WeakRef<Channel> | Channel
function getChannel(name) {
  const entry = channels.get(name);
  if (entry === undefined) return undefined;
  return entry instanceof Channel ? entry : entry.deref();
}
function retain(channel) {
  channels.set(channel.name, channel);
}
function release(channel) {
  channels.set(channel.name, typeof WeakRef === "function" ? new WeakRef(channel) : channel);
}

function defaultTransform(data) {
  return data;
}

function wrapStoreRun(store, data, next, transform = defaultTransform) {
  return () => {
    let context;
    try {
      context = transform(data);
    } catch (err) {
      process.nextTick(() => {
        throw err;
      });
      return next();
    }
    return store.run(context, next);
  };
}

class Channel {
  constructor(name) {
    this._subscribers = undefined;
    this._stores = undefined;
    this.name = name;
  }
  get hasSubscribers() {
    return (this._subscribers?.length ?? 0) > 0 || (this._stores?.size ?? 0) > 0;
  }
  subscribe(subscription) {
    validateFunction(subscription, "subscription");
    this._subscribers = [...(this._subscribers ?? []), subscription];
    retain(this);
  }
  unsubscribe(subscription) {
    const list = this._subscribers ?? [];
    const index = list.indexOf(subscription);
    if (index === -1) return false;
    this._subscribers = [...list.slice(0, index), ...list.slice(index + 1)];
    if (!this.hasSubscribers) release(this);
    return true;
  }
  bindStore(store, transform) {
    if (!this._stores) this._stores = new Map();
    this._stores.set(store, transform);
    retain(this);
  }
  unbindStore(store) {
    if (!this._stores?.has(store)) return false;
    this._stores.delete(store);
    if (!this.hasSubscribers) release(this);
    return true;
  }
  publish(data) {
    const subscribers = this._subscribers;
    if (!subscribers) return;
    for (const onMessage of subscribers) {
      try {
        onMessage(data, this.name);
      } catch (err) {
        process.nextTick(() => {
          throw err;
        });
      }
    }
  }
  runStores(data, fn, thisArg, ...args) {
    let run = () => {
      this.publish(data);
      return Reflect.apply(fn, thisArg, args);
    };
    for (const [store, transform] of this._stores ?? []) run = wrapStoreRun(store, data, run, transform);
    return run();
  }
}

function channel(name) {
  const existing = getChannel(name);
  if (existing) return existing;
  if (typeof name !== "string" && typeof name !== "symbol") {
    throw codeError(TypeError, "ERR_INVALID_ARG_TYPE", `The "channel" argument must be one of type string or symbol. Received ${internal.describeReceived(name)}`);
  }
  const ch = new Channel(name);
  release(ch);
  return ch;
}

function subscribe(name, subscription) {
  channel(name).subscribe(subscription);
}

function unsubscribe(name, subscription) {
  return channel(name).unsubscribe(subscription);
}

function hasSubscribers(name) {
  const ch = getChannel(name);
  return ch ? ch.hasSubscribers : false;
}

const traceEvents = ["start", "end", "asyncStart", "asyncEnd", "error"];

function assertChannel(value, name) {
  if (!(value instanceof Channel)) throw codeError(TypeError, "ERR_INVALID_ARG_TYPE", `The "${name}" argument must be an instance of Channel. Received ${internal.describeReceived(value)}`);
}

class TracingChannel {
  constructor(nameOrChannels) {
    if (typeof nameOrChannels === "string") {
      for (const eventName of traceEvents) this[eventName] = channel(`tracing:${nameOrChannels}:${eventName}`);
    } else if (nameOrChannels !== null && typeof nameOrChannels === "object") {
      for (const eventName of traceEvents) {
        const ch = nameOrChannels[eventName];
        assertChannel(ch, `nameOrChannels.${eventName}`);
        this[eventName] = ch;
      }
    } else {
      throw codeError(TypeError, "ERR_INVALID_ARG_TYPE", `The "nameOrChannels" argument must be one of type string or object. Received ${internal.describeReceived(nameOrChannels)}`);
    }
  }
  get hasSubscribers() {
    return traceEvents.some((e) => this[e].hasSubscribers);
  }
  subscribe(handlers) {
    for (const name of traceEvents) if (handlers[name]) this[name].subscribe(handlers[name]);
  }
  unsubscribe(handlers) {
    let done = true;
    for (const name of traceEvents) if (handlers[name] && !this[name].unsubscribe(handlers[name])) done = false;
    return done;
  }
  traceSync(fn, context = {}, thisArg, ...args) {
    if (!this.hasSubscribers) return Reflect.apply(fn, thisArg, args);
    const { start, end, error } = this;
    return start.runStores(context, () => {
      try {
        const result = Reflect.apply(fn, thisArg, args);
        context.result = result;
        return result;
      } catch (err) {
        context.error = err;
        error.publish(context);
        throw err;
      } finally {
        end.publish(context);
      }
    });
  }
  tracePromise(fn, context = {}, thisArg, ...args) {
    if (!this.hasSubscribers) return Reflect.apply(fn, thisArg, args);
    const { start, end, asyncStart, asyncEnd, error } = this;
    function reject(err) {
      context.error = err;
      error.publish(context);
      asyncStart.publish(context);
      asyncEnd.publish(context);
      return Promise.reject(err);
    }
    function resolve(result) {
      context.result = result;
      asyncStart.publish(context);
      asyncEnd.publish(context);
      return result;
    }
    return start.runStores(context, () => {
      try {
        let promise = Reflect.apply(fn, thisArg, args);
        if (!(promise instanceof Promise)) promise = Promise.resolve(promise);
        return promise.then(resolve, reject);
      } catch (err) {
        context.error = err;
        error.publish(context);
        throw err;
      } finally {
        end.publish(context);
      }
    });
  }
  traceCallback(fn, position = -1, context = {}, thisArg, ...args) {
    if (!this.hasSubscribers) return Reflect.apply(fn, thisArg, args);
    const { start, end, asyncStart, asyncEnd, error } = this;
    function wrappedCallback(err, res) {
      if (err) {
        context.error = err;
        error.publish(context);
      } else {
        context.result = res;
      }
      asyncStart.runStores(context, () => {
        try {
          return Reflect.apply(callback, this, arguments);
        } finally {
          asyncEnd.publish(context);
        }
      });
    }
    const index = position < 0 ? args.length + position : position;
    const callback = args[index];
    validateFunction(callback, "callback");
    args.splice(index, 1, wrappedCallback);
    return start.runStores(context, () => {
      try {
        return Reflect.apply(fn, thisArg, args);
      } catch (err) {
        context.error = err;
        error.publish(context);
        throw err;
      } finally {
        end.publish(context);
      }
    });
  }
}

function tracingChannel(nameOrChannels) {
  return new TracingChannel(nameOrChannels);
}

// Built-in channels published by other modules (cheap when nobody subscribes).
internal.dcPublish = (name, data) => {
  const ch = getChannel(name);
  if (ch && ch.hasSubscribers) ch.publish(data);
};

const dc = { channel, hasSubscribers, subscribe, tracingChannel, unsubscribe, Channel };
internal.diagnosticsChannel = dc;
internal.defineBuiltin("diagnostics_channel", () => dc);
