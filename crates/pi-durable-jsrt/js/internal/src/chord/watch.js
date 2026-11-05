// pi-durable:internal/src/chord/watch — Chord's watch handles (`Watch`, `NativeWatch`).
//
// The handle is the JS end of a watch: it owns the listener and delivers frames on the JS job
// queue (never inline, serialized, bounded backlog), so it stays JS. Native sources (durable
// document and graph watches) feed it frames through `advance`/`retire`/`end`.

import { withoutAbortSignal } from "pi-durable:internal/chord/context";

const MAX_PENDING_WATCH_FRAMES = 100;
const RETIREMENT_OPERATIONS = Object.freeze([Object.freeze(["r", null])]);
const NO_OPS = Object.freeze([]);

class WatchLifecycle {
  #detach;
  #report;
  #replace;
  #closed = Promise.withResolvers();
  #pending = [];
  #value;
  #listener;
  #started = false;
  #scheduled = false;
  #running = false;
  #retired = false;
  #end;
  #cancellationSignal;
  #cancellationListener;

  constructor(value, detach, report, replace) {
    this.#value = value;
    this.#detach = detach;
    this.#report = report;
    this.#replace = replace;
  }

  get value() {
    return this.#value;
  }

  get closed() {
    return this.#closed.promise;
  }

  start(listener) {
    if (this.#started) throw new Error("Watch is already started");
    if (this.#end !== undefined) throw new Error("Watch is stopped");
    this.#started = true;
    this.#listener = listener;
    if (this.#pending.length > 0) this.#schedule();
  }

  stop() {
    this.#terminate({ reason: "stopped" });
    return this.#closed.promise;
  }

  observeCancellation(signal) {
    if (this.#cancellationSignal !== undefined) throw new Error("Watch cancellation is already installed");
    if (this.#end !== undefined) return;
    this.#cancellationSignal = signal;
    this.#cancellationListener = () => this.cancel();
    signal.addEventListener("abort", this.#cancellationListener, { once: true });
    if (signal.aborted) this.cancel();
  }

  cancel() {
    this.#terminate({ reason: "cancelled" });
  }

  end(failure) {
    this.#terminate(failure === undefined ? { reason: "ended" } : { reason: "failed", error: failure.error });
  }

  enqueue(value, ops, context) {
    if (this.#end !== undefined || this.#retired) return;
    if (this.#pending.length >= MAX_PENDING_WATCH_FRAMES) {
      this.#pending.length = 0;
      const replacement = this.#replace?.() ?? value;
      this.#pending.push({ value: replacement, ops: this.overflowOps(replacement), context });
    } else {
      this.#pending.push({ value, ops, context });
    }
    if (this.#started) this.#schedule();
  }

  enqueueRetirement(value, ops, context) {
    if (this.#end !== undefined || this.#retired) return;
    this.#retired = true;
    if (this.#pending.length >= MAX_PENDING_WATCH_FRAMES) this.#pending.length = 0;
    this.#pending.push({ value, ops, context, retires: true });
    if (this.#started) this.#schedule();
  }

  #schedule() {
    if (this.#scheduled || this.#running || this.#end !== undefined) return;
    this.#scheduled = true;
    queueMicrotask(() => {
      this.#scheduled = false;
      void this.#drain();
    });
  }

  async #drain() {
    if (this.#running || this.#end !== undefined || !this.#started) return;
    this.#running = true;
    try {
      while (this.#end === undefined) {
        const frame = this.#pending.shift();
        const listener = this.#listener;
        if (frame === undefined || listener === undefined) break;
        this.#value = frame.value;
        const deliveryContext = withoutAbortSignal(frame.context);
        try {
          await this.invoke(listener, frame, deliveryContext);
        } catch (error) {
          if (this.#end === undefined) {
            this.#report?.(error);
            this.#terminate({ reason: "listener_error", error: toError(error) });
          }
          break;
        }
        if (frame.retires === true) {
          this.#terminate({ reason: "retired" });
          break;
        }
      }
    } finally {
      this.#running = false;
      if (this.#end === undefined && this.#pending.length > 0) this.#schedule();
    }
  }

  #terminate(end) {
    if (this.#end !== undefined) return;
    this.#end = end;
    const detach = this.#detach;
    this.#detach = undefined;
    this.#report = undefined;
    this.#replace = undefined;
    this.#listener = undefined;
    this.#pending.length = 0;
    if (this.#cancellationListener !== undefined) {
      this.#cancellationSignal?.removeEventListener("abort", this.#cancellationListener);
    }
    this.#cancellationSignal = undefined;
    this.#cancellationListener = undefined;
    detach?.();
    this.#closed.resolve(end);
  }
}

function toError(error) {
  if (error instanceof Error) return error;
  try {
    return new Error(String(error));
  } catch {
    return new Error(Object.prototype.toString.call(error));
  }
}

/** The watch handle of a JSON source: exact frames in order; an overflow becomes `[["r", newest]]`. */
export class Watch extends WatchLifecycle {
  advance(value, ops, context) {
    this.enqueue(value, ops, context);
  }

  retire(context) {
    this.enqueueRetirement(null, RETIREMENT_OPERATIONS, context);
  }

  invoke(listener, frame, context) {
    return listener(frame.value, frame.ops, context);
  }

  overflowOps(value) {
    return [["r", value]];
  }
}

/** The watch handle of a native source: values in order; an overflow collapses to the newest value. */
export class NativeWatch extends WatchLifecycle {
  constructor(value, detach, report) {
    super(value, detach, report, undefined);
  }

  advance(value, context) {
    this.enqueue(value, NO_OPS, context);
  }

  invoke(listener, frame, context) {
    return listener(frame.value, context);
  }

  overflowOps() {
    return NO_OPS;
  }
}
