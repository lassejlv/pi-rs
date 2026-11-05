// pi-durable:internal/chord/context — the JS `Context` of @earendil-works/chord/context over the
// native pi_chord::context::Context (src/dispatch/chord/context.rs `context.*`).
//
// Every JS context owns a native context: values are stored natively as handles, so `value(key)`
// returns the very value stored, and `toString()` is the native chain. The JS `AbortSignal` and
// telemetry span stay JS objects (identity is observable); caller cancellation is mirrored into a
// native AbortController, so `nativeContext(context)` hands Rust services a context that cancels
// with the JS one.

import { call, ref } from "pi-durable:bridge";
// The embedded pi-telemetry (js/pi-telemetry): its shared no-op context, whose identity is
// observable (`context.telemetry === NOOP_TELEMETRY_CONTEXT`).
import { NOOP_TELEMETRY_CONTEXT } from "@earendil-works/pi-telemetry";

const nativeKeys = new Map();

function nativeKey(key, create) {
  const token = key?.token;
  let native = nativeKeys.get(token);
  if (native === undefined && create) {
    native = call("context.key", typeof token === "symbol" ? token.description : undefined);
    nativeKeys.set(token, native);
  }
  return native;
}

const TELEMETRY_KEY = Object.freeze({ token: Symbol("chord.telemetry") });

class ChordContext {
  #native;
  #signal;
  #telemetry;
  /** A foreign (non-Chord) Context this one derives from: keys it does not bind resolve there. */
  #foreign;

  constructor(native, signal, telemetry, foreign) {
    this.#native = native;
    this.#signal = signal;
    this.#telemetry = telemetry;
    this.#foreign = foreign;
  }

  get abortSignal() {
    return this.#signal;
  }

  get telemetry() {
    return this.#telemetry ?? NOOP_TELEMETRY_CONTEXT;
  }

  value(key) {
    const native = nativeKey(key, false);
    const bound = native === undefined ? undefined : call("context.value", this.#native, native);
    if (bound !== undefined) return bound[0];
    return this.#foreign?.value(key);
  }

  toString() {
    return call("context.toString", this.#native);
  }

  static native(context) {
    return context.#native;
  }
  static signal(context) {
    return context.#signal;
  }
  static telemetrySpan(context) {
    return context.#telemetry;
  }
  static foreign(context) {
    return context.#foreign;
  }
  static is(context) {
    return context !== null && typeof context === "object" && #native in context;
  }
}

/** The native context behind `context` (a foreign Context gets one built from its signal). */
export function nativeContext(context) {
  if (ChordContext.is(context)) return ChordContext.native(context);
  const background = ChordContext.native(BACKGROUND_CONTEXT);
  const signal = context?.abortSignal;
  return signal === undefined ? background : mirrorSignal(background, signal);
}

/** A native context cancelled when `signal` aborts. */
function mirrorSignal(parent, signal) {
  const controller = call("context.controller");
  const derived = call("context.withAbortSignal", parent, controller);
  const abort = () => call("context.abort", controller, reasonText(signal.reason));
  if (signal.aborted) abort();
  else signal.addEventListener("abort", abort, { once: true });
  return derived;
}

function reasonText(reason) {
  if (reason instanceof Error) return reason.message;
  try {
    return reason === undefined ? undefined : String(reason);
  } catch {
    return undefined;
  }
}

export const BACKGROUND_CONTEXT = new ChordContext(call("context.root", "BACKGROUND_CONTEXT"), undefined, undefined);
export const TODO_CONTEXT = new ChordContext(call("context.root", "TODO_CONTEXT"), undefined, undefined);

export function createContextKey(description) {
  return Object.freeze({ token: Symbol(description) });
}

/** `[native, signal, telemetry, foreign]` of a parent context; a foreign Context keeps its values. */
function parts(context) {
  if (ChordContext.is(context)) {
    return [
      ChordContext.native(context),
      ChordContext.signal(context),
      ChordContext.telemetrySpan(context),
      ChordContext.foreign(context),
    ];
  }
  return [nativeContext(context), context.abortSignal, context.telemetry, context];
}

/** Derive a context containing one additional or replaced value. */
export function withContextValue(key, value, parent) {
  const [native, signal, telemetry, foreign] = parts(parent);
  return new ChordContext(call("context.withValue", native, nativeKey(key, true), ref(value)), signal, telemetry, foreign);
}

/** Derive an explicit telemetry parent without changing cancellation or application values. */
export function withTelemetry(telemetry, context) {
  const [native, signal, , foreign] = parts(context);
  return new ChordContext(call("context.withValue", native, nativeKey(TELEMETRY_KEY, true), ref(telemetry)), signal, telemetry, foreign);
}

/** Run a child span with a derived context whose telemetry member is that span. */
export function traceIn(options, fn, context) {
  return context.telemetry.startSpan(options, (span) => fn(withTelemetry(span, context)));
}

/** Derive a context cancelled by either the parent signal or the supplied signal. */
export function withAbortSignal(signal, context) {
  const [native, parentSignal, telemetry, foreign] = parts(context);
  const combined = parentSignal === undefined ? signal : AbortSignal.any([parentSignal, signal]);
  return new ChordContext(mirrorSignal(native, signal), combined, telemetry, foreign);
}

/** Derive a context retaining all values except caller cancellation. */
export function withoutAbortSignal(context) {
  const [native, , telemetry, foreign] = parts(context);
  return new ChordContext(call("context.withoutAbortSignal", native), undefined, telemetry, foreign);
}

/** Derive an independently cancellable child context. */
export function withCancel(context) {
  const controller = new AbortController();
  return {
    context: withAbortSignal(controller.signal, context),
    cancel: (reason) => controller.abort(reason),
  };
}

/** Observe a promise until it settles or the invocation is cancelled. */
export function awaitWithContext(promise, context) {
  const signal = context.abortSignal;
  if (signal === undefined) return promise;
  if (signal.aborted) return Promise.reject(abortError(signal));
  return new Promise((resolve, reject) => {
    const onAbort = () => reject(abortError(signal));
    signal.addEventListener("abort", onAbort, { once: true });
    void promise.then(
      (value) => {
        signal.removeEventListener("abort", onAbort);
        resolve(value);
      },
      (error) => {
        signal.removeEventListener("abort", onAbort);
        reject(error);
      },
    );
  });
}

function abortError(signal) {
  const reason = signal.reason;
  return reason instanceof Error ? reason : new DOMException("The operation was aborted", "AbortError");
}
