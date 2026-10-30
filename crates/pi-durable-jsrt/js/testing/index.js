// @earendil-works/pi-durable/testing (packages/durable/src/testing/index.ts). The conformance cases
// are pi_durable::testing's (src/dispatch/testing.rs `testing.*`); these drivers register them with
// the caller's Vitest/Jest-compatible runner and run each against the caller's provider.

import { call, callAsync, ref } from "pi-durable:bridge";
import { describeEnv } from "pi-durable:internal/env/port";
import { BACKGROUND_CONTEXT } from "@earendil-works/chord/context";

/** Adapts a Vitest/Jest-compatible `expect` function without importing either runner. */
export function createExpectAssertions(expect) {
  const result = (actual, message) => expect(actual, message);
  return {
    ok(value, message) {
      result(value, message).toBeTruthy();
    },
    strictEqual(actual, expected) {
      result(actual).toBe(expected);
    },
    deepEqual(actual, expected) {
      result(actual).toEqual(expected);
    },
    partialDeepEqual(actual, expected) {
      result(actual).toMatchObject(expected);
    },
    greaterThan(actual, expected) {
      result(actual).toBeGreaterThan(expected);
    },
    async rejects(operation, messageIncludes) {
      await result(operation).rejects.toThrow(messageIncludes);
    },
  };
}

const port = {
  provide(withEnv, pending) {
    return withEnv((env) => callAsync("testing.envUse", pending, describeEnv(env)));
  },
};

function envOptions(options) {
  return {
    ...(options.shell === undefined ? {} : { shell: [...options.shell] }),
    ...(options.symlinks === undefined ? {} : { symlinks: options.symlinks }),
    ...(options.listFiles === undefined ? {} : { listFiles: options.listFiles }),
  };
}

/**
 * Creates runner-independent cases for an `ExecutionEnv`. `withEnv` must call and await its callback exactly once per
 * case with an environment whose `cwd` is a fresh, empty, writable directory.
 */
export function createEnvConformance(options) {
  const wire = envOptions(options);
  return call("testing.envCases", wire).map((entry, index) => ({
    name: entry.name,
    ...(entry.timeoutMs === undefined ? {} : { timeoutMs: entry.timeoutMs }),
    run: () => callAsync("testing.envRun", index, wire, ref(port), ref(options.withEnv)),
  }));
}

/** Registers the runner-independent `ExecutionEnv` cases with a Vitest/Jest-compatible test runner. */
export function registerEnvConformance(runner, name, withEnv, options = {}) {
  const cases = createEnvConformance({ ...options, assertions: createExpectAssertions(runner.expect), withEnv });
  runner.describe(name, () => {
    for (const testCase of cases) runner.it(testCase.name, testCase.run, testCase.timeoutMs);
  });
}

const backends = {
  MemoryStorage: "memory",
  JsonlStorage: "jsonl",
  SqliteStorage: "sqlite",
  NativeSqliteStorage: "sqlite",
  IndexedDbStorage: "indexeddb",
};

function kindOf(storage) {
  for (let proto = Object.getPrototypeOf(storage); proto !== null; proto = Object.getPrototypeOf(proto)) {
    const kind = backends[proto.constructor?.name];
    if (kind !== undefined) return kind;
  }
  return undefined;
}

/**
 * The recording to replay against `storage`: error wording and ID reuse differ per backend, so the
 * reference was recorded once per backend, and once more through a wrapper that reopens the
 * backend after every commit (the tests' `ReopeningStorage`, a storage that holds a backend
 * instead of being one). Anything else replays the memory recording.
 */
function backendOf(storage) {
  const kind = kindOf(storage);
  if (kind !== undefined) return kind;
  for (const value of Object.values(storage)) {
    const inner = value !== null && typeof value === "object" ? kindOf(value) : undefined;
    if (inner !== undefined && inner !== "memory") return `${inner}-reopen`;
  }
  return "memory";
}

/** Replay one recorded case against `storage` through its own methods, as the reference's runner recorded it. */
async function replay(assert, storage, index) {
  const requests = call("testing.storageTrace", backendOf(storage), index);
  for (const [position, request] of requests.entries()) {
    const adopted = [];
    const label = `request ${position} ${request.method}`;
    let value;
    let error;
    let failed = false;
    try {
      value =
        request.method === "commit"
          ? await storage.commit(request.args[0], (seq) => adopted.push(seq), BACKGROUND_CONTEXT)
          : await storage[request.method](...request.args, BACKGROUND_CONTEXT);
    } catch (thrown) {
      failed = true;
      error = thrown;
    }
    if (request.error !== undefined) {
      assert.ok(failed, `${label}: expected ${request.error.name}: ${request.error.message}`);
      assert.strictEqual(`${label}: ${error?.name}: ${error?.message}`, `${label}: ${request.error.name}: ${request.error.message}`);
    } else {
      if (failed) throw error;
      assert.deepEqual({ request: label, value }, { request: label, value: request.missing === true ? undefined : request.value });
    }
    assert.deepEqual({ request: label, adopted }, { request: label, adopted: request.adopted });
  }
}

/** Creates the runner-independent `Storage` cases; `withStorage` calls its callback once per case with a fresh storage. */
export function createStorageConformance(options) {
  return call("testing.storageCases", "memory").map((name, index) => ({
    name,
    run: () => options.withStorage((storage) => replay(options.assertions, storage, index)),
  }));
}

/** Registers the runner-independent cases with a Vitest/Jest-compatible test runner. */
export function registerStorageConformance(runner, name, withStorage) {
  const cases = createStorageConformance({ assertions: createExpectAssertions(runner.expect), withStorage });
  runner.describe(name, () => {
    for (const testCase of cases) runner.it(testCase.name, testCase.run);
  });
}

// Storage benchmark scales (pi_durable::testing::storage_benchmark).
const scales = call("testing.benchmarkScales");
const scaleOf = (native) => ({ name: native.name, entryCount: native.entryCount, taskCount: native.taskCount, documentCount: native.documentCount });
// The records every seeded dataset has besides its entries, tasks and documents (root, replay
// tails, forks), from the native count.
const fixedRecords =
  scales.timing.primaryRecordCount - scales.timing.entryCount - scales.timing.taskCount - scales.timing.documentCount;

export const STORAGE_MEMORY_SCALES = scales.memory.map(scaleOf);
export const TIMING_SCALE = scaleOf(scales.timing);

export function storageBenchmarkPrimaryRecordCount(scale) {
  return fixedRecords + scale.entryCount + scale.taskCount + scale.documentCount;
}

// Storage benchmarks: the reference's drivers over the storage's own methods
// (packages/durable/src/testing/storage-benchmark.ts), checked at load against the native lists
// (pi_durable::testing::storage_{read,write}_benchmarks): same names, order and write counts.
const ROOT = 1;
const adopt = () => {};
const REPLAY_TAILS = [0, 16, 128, 1024];
const HISTORY_SEGMENT_LENGTH = 128;
const FORK_DEPTH = 8;
const ENTRIES_PER_FORK = 32;
const BATCH_SIZE = 100;

function task(id, index) {
  const statuses = ["pending", "running", "terminal"];
  const status = statuses[index % statuses.length];
  const common = {
    id,
    conversationId: ROOT,
    kind: index % 4 === 0 ? "benchmark.filtered" : "benchmark.other",
    version: 1,
    input: { index },
    background: index % 5 === 0,
    abortRequested: index % 7 === 0,
  };
  if (status === "terminal") return { ...common, state: { status, outcome: { status: "completed", result: { index } } } };
  return { ...common, state: { status, checkpoint: { index, payload: "x".repeat(64) } } };
}

const commit = (storage, writes) => storage.commit(writes, adopt, BACKGROUND_CONTEXT);
const delta = (id, count) => ({ type: "document.change", id, content: { kind: "delta", version: 1, ops: [["s", ["count"], count]] } });

/** Seed deterministic representative data through only the public `Storage` contract. */
export async function seedStorageBenchmark(storage, scale = TIMING_SCALE) {
  await commit(storage, [{ type: "conversation", value: { id: ROOT } }]);
  let firstEntryId;
  for (let start = 0; start < scale.entryCount; start += BATCH_SIZE) {
    const writes = [];
    for (let index = start; index < Math.min(start + BATCH_SIZE, scale.entryCount); index++) {
      const id = await storage.mintId();
      if (index === 0) firstEntryId = id;
      writes.push({
        type: "entry",
        value: {
          id,
          conversationId: ROOT,
          kind: "benchmark.entry",
          ...(index === 0 ? { head: id } : {}),
          data: { index, text: `entry-${index}-${"x".repeat(96)}` },
        },
      });
    }
    await commit(storage, writes);
  }
  for (let start = 0; start < scale.taskCount; start += BATCH_SIZE) {
    const writes = [];
    for (let index = start; index < Math.min(start + BATCH_SIZE, scale.taskCount); index++) {
      writes.push({ type: "task", value: task(await storage.mintId(), index) });
    }
    await commit(storage, writes);
  }
  let exactDocumentId;
  for (let start = 0; start < scale.documentCount; start += BATCH_SIZE) {
    const writes = [];
    for (let index = start; index < Math.min(start + BATCH_SIZE, scale.documentCount); index++) {
      const id = await storage.mintId();
      exactDocumentId = id;
      writes.push({
        type: "document.create",
        record: { id, kind: "benchmark.family", key: `key-${index}`, scope: { kind: "session" } },
        content: { kind: "base", version: 1, value: { index, text: "x".repeat(128) } },
      });
    }
    await commit(storage, writes);
  }
  const replayEntries = await Promise.all(REPLAY_TAILS.map(async (tail) => ({ tail, id: await storage.mintId() })));
  await commit(
    storage,
    replayEntries.map(({ tail, id }) => ({
      type: "document.create",
      record: {
        id,
        kind: "benchmark.replay",
        key: String(tail),
        scope: { kind: "conversation", conversationId: ROOT },
        history: "rewindable",
        fork: "asOf",
      },
      content: { kind: "base", version: 1, value: { count: 0, text: "x".repeat(64) } },
    })),
  );
  for (let count = 1; count <= REPLAY_TAILS.at(-1); count++) {
    await commit(storage, replayEntries.filter(({ tail }) => count <= tail).map(({ id }) => delta(id, count)));
  }
  const historicalDocumentId = await storage.mintId();
  await commit(storage, [
    {
      type: "document.create",
      record: { id: historicalDocumentId, kind: "benchmark.history", scope: { kind: "conversation", conversationId: ROOT }, history: "rewindable", fork: "asOf" },
      content: { kind: "base", version: 1, value: { count: 0 } },
    },
  ]);
  let ancientAt;
  for (let count = 1; count <= HISTORY_SEGMENT_LENGTH; count++) ancientAt = await commit(storage, [delta(historicalDocumentId, count)]);
  await commit(storage, [{ type: "document.change", id: historicalDocumentId, content: { kind: "base", version: 1, value: { count: HISTORY_SEGMENT_LENGTH } } }]);
  if (ancientAt === undefined) throw new Error("Benchmark history seed produced no commits");
  let recentAt = ancientAt;
  for (let count = HISTORY_SEGMENT_LENGTH + 1; count <= HISTORY_SEGMENT_LENGTH * 2; count++) {
    recentAt = await commit(storage, [delta(historicalDocumentId, count)]);
  }
  if (firstEntryId === undefined) throw new Error("Benchmark scale must create entries");
  let parentConversationId = ROOT;
  let parentAt = firstEntryId;
  let deepestConversationId = ROOT;
  for (let depth = 0; depth < FORK_DEPTH; depth++) {
    const conversationId = await storage.mintId();
    await commit(storage, [{ type: "conversation", value: { id: conversationId, parent: { conversationId: parentConversationId, at: parentAt } } }]);
    const ids = await Promise.all(Array.from({ length: ENTRIES_PER_FORK }, () => storage.mintId()));
    await commit(
      storage,
      ids.map((id, index) => ({ type: "entry", value: { id, conversationId, kind: "benchmark.fork", data: { depth, index } } })),
    );
    parentConversationId = conversationId;
    parentAt = ids.at(-1);
    deepestConversationId = conversationId;
  }
  if (exactDocumentId === undefined) throw new Error("Benchmark scale must create documents");
  return {
    firstEntryId,
    filteredTaskCount: Math.min(50, Math.ceil(scale.taskCount / 60)),
    exactDocumentId,
    exactDocumentKey: `key-${scale.documentCount - 1}`,
    replayDocumentIds: Object.fromEntries(replayEntries.map(({ tail, id }) => [tail, id])),
    historicalDocumentId,
    ancientAt,
    recentAt,
    deepestConversationId,
    ancestorHeadEntryId: firstEntryId,
  };
}

const count = (document) => Number(document?.value.count);

export const STORAGE_READ_BENCHMARKS = [
  {
    name: "exact entry lookup",
    async run(storage, dataset) {
      return (await storage.entry(dataset.firstEntryId, BACKGROUND_CONTEXT))?.entry.id ?? -1;
    },
    expected: ({ firstEntryId }) => firstEntryId,
  },
  {
    name: "entry page scan (100)",
    async run(storage) {
      return (await storage.scanEntries({ conversationId: ROOT }, 100, undefined, BACKGROUND_CONTEXT)).items.length;
    },
    expected: () => 100,
  },
  {
    name: "filtered task scan (50)",
    async run(storage) {
      const filter = { kind: "benchmark.filtered", status: "pending", background: true };
      return (await storage.scanTasks(filter, 50, undefined, BACKGROUND_CONTEXT)).items.length;
    },
    expected: ({ filteredTaskCount }) => filteredTaskCount,
  },
  {
    name: "exact document address among many",
    async run(storage, dataset) {
      const address = { kind: "benchmark.family", key: dataset.exactDocumentKey, scope: { kind: "session" } };
      return (await storage.findDocument(address, "current", BACKGROUND_CONTEXT))?.id ?? -1;
    },
    expected: ({ exactDocumentId }) => exactDocumentId,
  },
  ...REPLAY_TAILS.map((tail) => ({
    name: `document replay tail (${tail})`,
    async run(storage, dataset) {
      return count(await storage.document(dataset.replayDocumentIds[tail], "current", BACKGROUND_CONTEXT));
    },
    expected: () => tail,
  })),
  {
    name: "ancient historical read before newer base",
    async run(storage, dataset) {
      return count(await storage.document(dataset.historicalDocumentId, dataset.ancientAt, BACKGROUND_CONTEXT));
    },
    expected: () => HISTORY_SEGMENT_LENGTH,
  },
  {
    name: "recent historical read after newer base",
    async run(storage, dataset) {
      return count(await storage.document(dataset.historicalDocumentId, dataset.recentAt, BACKGROUND_CONTEXT));
    },
    expected: () => HISTORY_SEGMENT_LENGTH * 2,
  },
  {
    name: "fork-depth history scan (100)",
    async run(storage, dataset) {
      return (await storage.scanEntries({ conversationId: dataset.deepestConversationId }, 100, undefined, BACKGROUND_CONTEXT)).items.length;
    },
    expected: () => 100,
  },
  {
    name: "fork-depth head lookup",
    async run(storage, dataset) {
      return (await storage.findLatestHeadMarker(dataset.deepestConversationId, undefined, BACKGROUND_CONTEXT))?.id ?? -1;
    },
    expected: ({ ancestorHeadEntryId }) => ancestorHeadEntryId,
  },
];

/** Seed the common state expected by every write benchmark sample. */
export async function seedStorageWriteBenchmark(storage) {
  await commit(storage, [{ type: "conversation", value: { id: ROOT } }]);
  await commit(
    storage,
    await Promise.all(
      Array.from({ length: 100 }, async (_, index) => ({
        type: "entry",
        value: { id: await storage.mintId(), conversationId: ROOT, kind: "benchmark.baseline", data: { index } },
      })),
    ),
  );
}

export const STORAGE_WRITE_BENCHMARKS = [
  {
    name: "commit one entry",
    expected: 1,
    async run(storage) {
      const id = await storage.mintId();
      await commit(storage, [{ type: "entry", value: { id, conversationId: ROOT, kind: "benchmark.write", data: { text: "x".repeat(128) } } }]);
      return 1;
    },
  },
  {
    name: "commit 100 entries",
    expected: 100,
    async run(storage) {
      const writes = await Promise.all(
        Array.from({ length: 100 }, async (_, index) => ({
          type: "entry",
          value: { id: await storage.mintId(), conversationId: ROOT, kind: "benchmark.write", data: { index, text: "x".repeat(128) } },
        })),
      );
      await commit(storage, writes);
      return writes.length;
    },
  },
  {
    name: "commit mixed entry/task/submission/document",
    expected: 4,
    async run(storage) {
      const entryId = await storage.mintId();
      const taskId = await storage.mintId();
      const submissionId = await storage.mintId();
      const documentId = await storage.mintId();
      const writes = [
        { type: "entry", value: { id: entryId, conversationId: ROOT, kind: "benchmark.mixed" } },
        { type: "task", value: task(taskId, taskId) },
        {
          type: "submission",
          value: { id: submissionId, conversationId: ROOT, requestId: `benchmark-${submissionId}`, type: "write", status: "done", entry: entryId },
        },
        {
          type: "document.create",
          record: { id: documentId, kind: "benchmark.mixed", key: String(documentId), scope: { kind: "session" } },
          content: { kind: "base", version: 1, value: { entryId, taskId } },
        },
      ];
      await commit(storage, writes);
      return writes.length;
    },
  },
];

{
  const native = call("testing.benchmarkDefinitions");
  const reads = STORAGE_READ_BENCHMARKS.map((b) => b.name);
  const writes = STORAGE_WRITE_BENCHMARKS.map((b) => `${b.name}=${b.expected}`);
  if (JSON.stringify(reads) !== JSON.stringify(native.reads) || JSON.stringify(writes) !== JSON.stringify(native.writes.map((w) => `${w.name}=${w.expected}`))) {
    throw new Error("pi-durable/testing: storage benchmarks differ from the native definitions");
  }
}
