// @earendil-works/chord/output over pi_chord::output (src/dispatch/chord.rs `output.*`).
// `Progress` is the upstream class (packages/chord/src/output.ts): it paces on the JS clock and
// timers it is given, so it stays JS.

import { call } from "pi-durable:bridge";

const constants = call("output.constants");

/** Remove control characters that break display and transcripts; tabs and newlines stay. */
export function sanitizeOutput(text) {
  return call("output.sanitize", String(text));
}

/** An exact slice of `text` within `limits`, and what it left out. */
export function boundOutput(text, limits) {
  return call("output.bound", text, { maxBytes: limits.maxBytes, maxLines: limits.maxLines, retain: limits.retain });
}

/** The end of the UTF-8 character containing byte `index` of `bytes`. */
export function characterEnd(bytes, index) {
  return call("output.characterEnd", bytes, index);
}

/** UTF-8 byte length of `content`; a lone surrogate counts as U+FFFD, three bytes. */
export function utf8ByteLength(content) {
  return call("output.utf8ByteLength", content);
}

/** Bounded running output of one stream (native `OutputBuffer`). */
export class OutputBuffer {
  #native;

  constructor(limits) {
    this.#native = call("output.bufferNew", { maxBytes: limits.maxBytes, maxLines: limits.maxLines, retain: limits.retain });
  }

  /** Bytes currently held; bounded by the limits plus one chunk. */
  get storedBytes() {
    return call("output.bufferStoredBytes", this.#native);
  }

  /** Accept a chunk; returns whether anything was accepted. */
  push(chunk, skipped) {
    const bytes = typeof chunk === "string" ? chunk : chunk instanceof Uint8Array ? chunk : new Uint8Array(chunk.buffer, chunk.byteOffset, chunk.byteLength);
    const skip =
      skipped === undefined ? undefined : { bytes: skipped.bytes, newlines: skipped.newlines, endsWithNewline: skipped.endsWithNewline };
    return call("output.bufferPush", this.#native, bytes, skip);
  }

  /** End the stream: an incomplete trailing character becomes U+FFFD. */
  end() {
    call("output.bufferEnd", this.#native);
  }

  /** The retained output and what the limits dropped. */
  snapshot() {
    return call("output.bufferSnapshot", this.#native);
  }
}

/** Each progress commit also buys a pause proportional to what it wrote. */
export const PROGRESS_BYTES_PER_SECOND = constants.PROGRESS_BYTES_PER_SECOND;

const SYSTEM_CLOCK = {
  now: () => Date.now(),
  setTimer: (callback, delayMs) => {
    const timer = setTimeout(callback, delayMs);
    return () => clearTimeout(timer);
  },
};

/**
 * Adaptive progress commits: the first change after an idle period commits at once; each commit then delays the next
 * by at least `minIntervalMs` and by its written size at 100 KiB/s. At most one commit is in flight; changes made
 * meanwhile coalesce into the next one. Paces on `clock`, by default `Date.now` and `setTimeout`.
 */
export class Progress {
  #write;
  #onError;
  #minIntervalMs;
  #clock;
  #waiters = [];
  #timer;
  #inFlight;
  #nextAt = 0;
  #dirty = false;
  #stopped = false;

  constructor(write, onError, minIntervalMs, clock = SYSTEM_CLOCK) {
    this.#write = write;
    this.#onError = onError;
    this.#minIntervalMs = minIntervalMs;
    this.#clock = clock;
  }

  /** Schedule a commit. */
  mark() {
    this.#dirty = true;
    this.#schedule();
  }

  /** Schedule a commit; the promise settles with the commit that includes this change. */
  markAndWait() {
    const waiter = Promise.withResolvers();
    this.#waiters.push(waiter);
    this.mark();
    return waiter.promise;
  }

  /** Stop committing and wait for the commit in flight; returns the waiters the final commit must settle. */
  async stop() {
    this.#stopped = true;
    this.#timer?.();
    this.#timer = undefined;
    await this.#inFlight;
    return this.#waiters.splice(0);
  }

  #schedule() {
    if (this.#stopped || this.#timer !== undefined || this.#inFlight !== undefined) return;
    const wait = this.#nextAt - this.#clock.now();
    if (wait <= 0) this.#flush();
    else
      this.#timer = this.#clock.setTimer(() => {
        this.#timer = undefined;
        // Checks the clock again: a timer that fires early waits for the rest.
        this.#schedule();
      }, wait);
  }

  #flush() {
    if (this.#stopped || !this.#dirty) return;
    this.#dirty = false;
    const waiters = this.#waiters.splice(0);
    const started = this.#clock.now();
    this.#inFlight = this.#write()
      .then(
        (bytes) => {
          this.#nextAt = started + Math.max(this.#minIntervalMs, (bytes * 1000) / PROGRESS_BYTES_PER_SECOND);
          for (const waiter of waiters) waiter.resolve();
        },
        (error) => {
          this.#nextAt = started + this.#minIntervalMs;
          for (const waiter of waiters) waiter.reject(error);
          this.#onError(error);
        },
      )
      .finally(() => {
        this.#inFlight = undefined;
        if (this.#dirty) this.#schedule();
      });
  }
}
