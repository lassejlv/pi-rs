// @earendil-works/pi-durable/env/node (packages/durable/src/env/node.ts): NodeExecutionEnv over
// the native environment (src/dispatch/env.rs `fs.*`, `reader.*`, `watch.*`). Paths, readers,
// decoding, shell processes, spill policy and watchers run in Rust. Opened text files and spill
// streams use this runtime's `node:fs` through the port below, as upstream's class does, so
// `FileHandle` spies and `vi.mock("node:fs")` still see every call.

import { createWriteStream } from "node:fs";
import { open, constants } from "node:fs/promises";
import { basename } from "node:path";
import { call, callAsync, ref, FileError, ExecutionError } from "pi-durable:bridge";
import { BACKGROUND_CONTEXT } from "@earendil-works/chord/context";
import { nativeContext } from "pi-durable:internal/chord/context";
import { ok, err } from "@earendil-works/pi-durable/env";

const nodeCodes = {
  ABORT_ERR: "aborted",
  ENOENT: "not_found",
  EACCES: "permission_denied",
  EPERM: "permission_denied",
  ENOTDIR: "not_directory",
  EISDIR: "is_directory",
  EINVAL: "invalid",
  ELOOP: "invalid",
  EMLINK: "invalid",
};

function failure(error, path) {
  return { ok: false, error: { code: nodeCodes[error?.code] ?? "unknown", message: String(error?.message ?? error), path: error?.path ?? path } };
}

async function hosted(run, path) {
  try {
    return { ok: true, value: await run() };
  } catch (error) {
    return failure(error, path);
  }
}

// The native environment's host capabilities: opened files and spill streams.
const port = {
  fileOpen(path, write, regular, noFollow) {
    return hosted(async () => {
      let flags = write ? constants.O_RDWR : constants.O_RDONLY;
      if (regular) flags |= constants.O_NONBLOCK ?? 0;
      if (noFollow) flags |= constants.O_NOFOLLOW ?? 0;
      return ref({ file: await open(path, flags), path });
    }, path);
  },
  fileInfo(file) {
    return hosted(async () => {
      const info = await file.file.stat();
      return {
        name: basename(file.path),
        path: file.path,
        kind: info.isFile() ? "file" : info.isDirectory() ? "directory" : info.isSymbolicLink() ? "symlink" : "other",
        size: info.size,
        mtimeMs: info.mtimeMs,
      };
    }, file.path);
  },
  fileRead(file, offset, length) {
    return hosted(async () => {
      const bytes = new Uint8Array(length);
      const { bytesRead } = await file.file.read(bytes, 0, length, offset);
      return bytes.subarray(0, bytesRead);
    }, file.path);
  },
  fileSync(file) {
    return hosted(() => file.file.sync(), file.path);
  },
  fileClose(file) {
    return hosted(() => file.file.close(), file.path);
  },
  spillOpen(path) {
    return hosted(() => {
      const stream = createWriteStream(path, { flags: "a", highWaterMark: 1024 * 1024 });
      let finish;
      const state = { stream, closed: new Promise((resolve) => (finish = resolve)) };
      stream.on("error", (error) => {
        state.error = error;
        finish(error);
      });
      stream.on("finish", () => finish());
      return ref(state);
    }, path);
  },
  spillWrite(state, bytes) {
    return hosted(() => {
      if (state.error) throw state.error;
      const accepted = state.stream.write(bytes);
      if (!accepted) {
        state.draining = new Promise((resolve) => {
          const done = (error) => {
            state.stream.off("drain", drain);
            state.stream.off("error", done);
            resolve(error);
          };
          const drain = () => done();
          state.stream.once("drain", drain);
          state.stream.once("error", done);
        });
      }
      return accepted;
    });
  },
  spillDrain(state) {
    return hosted(async () => {
      const error = state.error ?? (await state.draining);
      if (error) throw error;
    });
  },
  spillSettled(state) {
    return hosted(async () => {
      const error = await state.closed;
      if (error) throw error;
    });
  },
  spillFinish(state) {
    return hosted(async () => {
      state.stream.end();
      const error = await state.closed;
      if (error) throw error;
    });
  },
  async shellTemp(env) {
    const result = await env.createTempFile({ prefix: "pi-output-", suffix: ".log" }, BACKGROUND_CONTEXT);
    return result.ok ? result : { ok: false, error: { code: result.error.code, message: result.error.message, path: result.error.path } };
  },
};

// Native state lives under a module symbol, not `#private` fields: upstream's classes use TypeScript
// `private`, so `Object.create(env)` and similar wrappers work on them.
const NATIVE = Symbol("pi-durable native");
const CWD = Symbol("pi-durable cwd");

function hold(object, key, value) {
  Object.defineProperty(object, key, { value, writable: true, configurable: true });
}

/** Settle a native file operation as upstream's `Result`. */
async function fileResult(operation) {
  try {
    return ok(await operation);
  } catch (error) {
    if (error instanceof FileError) return err(error);
    throw error;
  }
}

function changeOf(change) {
  if (change.error === undefined) return change;
  const error = change.error;
  return { error: new FileError(error.code, error.message, error.path) };
}

function file(env, method, context, ...args) {
  return fileResult(callAsync(method, env[NATIVE], nativeContext(context), ...args));
}

class NodeTextLineReader {
  constructor(native) {
    hold(this, NATIVE, native);
  }

  readLine(context) {
    return fileResult(callAsync("reader.line", this[NATIVE], nativeContext(context)));
  }

  async close(context) {
    await callAsync("reader.close", this[NATIVE], nativeContext(context));
  }
}

class NodeBinaryReader {
  constructor(native) {
    hold(this, NATIVE, native);
  }

  info(context) {
    return fileResult(callAsync("reader.info", this[NATIVE], nativeContext(context)));
  }

  read(offset, length, context) {
    return fileResult(callAsync("reader.read", this[NATIVE], nativeContext(context), offset, length));
  }

  scanLines(options, context) {
    return fileResult(callAsync("reader.scan", this[NATIVE], nativeContext(context), options.startLine, options.endLine));
  }

  async close(context) {
    await callAsync("reader.close", this[NATIVE], nativeContext(context));
  }
}

class NodeDirReader {
  constructor(native) {
    hold(this, NATIVE, native);
  }

  next(maxEntries, context) {
    return fileResult(callAsync("reader.next", this[NATIVE], nativeContext(context), maxEntries));
  }

  async close(context) {
    await callAsync("reader.close", this[NATIVE], nativeContext(context));
  }
}

class NodeFileWatcher {
  constructor(native, mode) {
    hold(this, NATIVE, native);
    hold(this, "mode", mode);
  }

  async close(context) {
    await callAsync("watch.close", this[NATIVE], nativeContext(context));
  }
}

export class NodeExecutionEnv {
  /** Every local environment sees the same files. */
  id;

  constructor(options) {
    const opened = call(
      "fs.new",
      { cwd: options.cwd, shellPath: options.shellPath, shellEnv: options.shellEnv, watch: options.watch },
      ref(port),
    );
    hold(this, NATIVE, opened.native);
    hold(this, CWD, options.cwd);
    this.id = opened.id;
  }

  get cwd() {
    return this[CWD];
  }

  set cwd(path) {
    call("fs.setCwd", this[NATIVE], path);
    this[CWD] = path;
  }

  async watch(targets, onChange, context) {
    const deliver = (change) => onChange(changeOf(change));
    const opened = await file(this, "fs.watch", context, targets, deliver);
    return opened.ok ? ok(new NodeFileWatcher(opened.value.watcher, opened.value.mode)) : opened;
  }

  absolutePath(path, context) {
    return file(this, "fs.absolute", context, path);
  }

  joinPath(parts, context) {
    return file(this, "fs.join", context, parts);
  }

  async exec(command, options, context) {
    const onOutput = options?.onOutput;
    const output = onOutput === undefined ? undefined : (text, info) => onOutput(text, context, info);
    const host = { cwd: process.cwd(), platform: process.platform, env: { ...process.env } };
    try {
      return ok(
        await callAsync("fs.exec", this[NATIVE], nativeContext(context), ref(this), command, options ?? {}, output, host),
      );
    } catch (error) {
      if (error instanceof ExecutionError) return err(error);
      throw error;
    }
  }

  async openTextLineReader(path, context) {
    const opened = await file(this, "fs.openText", context, path);
    return opened.ok ? ok(new NodeTextLineReader(opened.value)) : opened;
  }

  readTextFile(path, context) {
    return file(this, "fs.readText", context, path);
  }

  async readTextLines(path, options, context) {
    if (options?.maxLines !== undefined && options.maxLines <= 0) return ok([]);
    const opened = await this.openTextLineReader(path, context);
    if (!opened.ok) return opened;
    const lines = [];
    try {
      while (options?.maxLines === undefined || lines.length < options.maxLines) {
        const line = await opened.value.readLine(context);
        if (!line.ok) return line;
        if (line.value === undefined) break;
        lines.push(line.value.text);
      }
      return ok(lines);
    } finally {
      await opened.value.close(context);
    }
  }

  readBinaryFile(path, context) {
    return file(this, "fs.readBinary", context, path);
  }

  async openBinaryReader(path, options, context) {
    const opened = await file(this, "fs.openBinary", context, path, options?.noFollow === true);
    return opened.ok ? ok(new NodeBinaryReader(opened.value)) : opened;
  }

  writeFile(path, content, context) {
    return file(this, "fs.write", context, path, content);
  }

  appendFile(path, content, context) {
    return file(this, "fs.append", context, path, content);
  }

  truncateFile(path, size, context) {
    return file(this, "fs.truncate", context, path, size);
  }

  flushFile(path, context) {
    return file(this, "fs.flush", context, path);
  }

  renameFile(sourcePath, destinationPath, context) {
    return file(this, "fs.rename", context, sourcePath, destinationPath);
  }

  fileInfo(path, context) {
    return file(this, "fs.info", context, path);
  }

  listDir(path, context) {
    return file(this, "fs.list", context, path);
  }

  async openDirReader(path, context) {
    const opened = await file(this, "fs.openDir", context, path);
    return opened.ok ? ok(new NodeDirReader(opened.value)) : opened;
  }

  canonicalPath(path, context) {
    return file(this, "fs.canonical", context, path);
  }

  async exists(path, context) {
    const result = await this.fileInfo(path, context);
    if (result.ok) return ok(true);
    if (result.error.code === "not_found") return ok(false);
    return err(result.error);
  }

  createDir(path, options, context) {
    return file(this, "fs.mkdir", context, path, options?.recursive ?? true);
  }

  remove(path, options, context) {
    return file(this, "fs.remove", context, path, options?.recursive ?? false, options?.force ?? false);
  }

  createTempDir(prefix, context) {
    return file(this, "fs.tempDir", context, prefix);
  }

  createTempFile(options, context) {
    return file(this, "fs.tempFile", context, options?.prefix, options?.suffix);
  }

  async cleanup(context) {
    await callAsync("fs.cleanup", this[NATIVE], nativeContext(context));
  }
}
