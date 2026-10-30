// @earendil-works/pi-durable/env (packages/durable/src/env/index.ts): the Result helpers and error
// classes, plus the decoders and line scanner over pi_durable::env (src/dispatch/env.rs `env.*`).
// Portable: no Node built-ins, nothing from env/node.

import { call, FileError, ExecutionError } from "pi-durable:bridge";

export { FileError, ExecutionError };

export function ok(value) {
  return { ok: true, value };
}

export function err(error) {
  return { ok: false, error };
}

export function getOrThrow(result) {
  if (!result.ok) throw result.error;
  return result.value;
}

export function getOrUndefined(result) {
  return result.ok ? result.value : undefined;
}

export function toError(error) {
  if (error instanceof Error) return error;
  if (typeof error === "string") return new Error(error);
  try {
    return new Error(JSON.stringify(error));
  } catch {
    return new Error(String(error));
  }
}

function bytesOf(input) {
  if (input instanceof Uint8Array) return input;
  if (ArrayBuffer.isView(input)) return new Uint8Array(input.buffer, input.byteOffset, input.byteLength);
  if (input instanceof ArrayBuffer) return new Uint8Array(input);
  throw new TypeError("The \"input\" argument must be an instance of ArrayBuffer or ArrayBufferView");
}

/** A UTF-8 decoder for a byte range (`TextDecoder` with `ignoreBOM: true`), over the native decoder. */
class RangeDecoder {
  #native = call("env.rangeDecoder");

  get encoding() {
    return "utf-8";
  }
  get fatal() {
    return false;
  }
  get ignoreBOM() {
    return true;
  }

  decode(input, options) {
    const bytes = input === undefined ? undefined : bytesOf(input);
    return call("env.rangeDecode", this.#native, bytes, options?.stream === true);
  }
}

/** A streaming decoder for a byte range; callers that start at the beginning of a file skip a leading mark themselves. */
export function rangeDecoder() {
  return new RangeDecoder();
}

/** Whether decoding the whole input drops its first three bytes as a byte-order mark. */
export function startsWithBom(firstBytes) {
  return firstBytes[0] === 0xef && firstBytes[1] === 0xbb && firstBytes[2] === 0xbf;
}

/** Decodes one stream chunk by chunk exactly like decoding all of it at once. */
export class StreamDecoder {
  #native = call("env.streamDecoder");

  decode(bytes) {
    return call("env.streamDecode", this.#native, bytes === undefined ? undefined : bytesOf(bytes));
  }
}

/** Line positions and decoded sizes of a byte stream (native `LineScanner`). */
export class LineScanner {
  #native;

  constructor(startLine, endLine = Number.POSITIVE_INFINITY) {
    this.#native = call("env.lineScanner", startLine, endLine);
  }

  push(chunk) {
    call("env.lineScannerPush", this.#native, bytesOf(chunk));
  }

  finish() {
    return call("env.lineScannerFinish", this.#native);
  }
}
