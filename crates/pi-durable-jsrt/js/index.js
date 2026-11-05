// @earendil-works/pi-durable: the `.` entry (packages/durable/src/index.ts). Documents, entries,
// errors, tasks, the Session and MemoryStorage are chunk C's glue over the native kernel; the
// Harness, registry, agent docs and tasks come from the Harness glue (pi-durable:internal/harness).
export { defineDoc, defineDocFamily } from "pi-durable:internal/src/durable/documents";
export {
  AssistantEntry,
  CompactionEntry,
  defineEntry,
  ResetEntry,
  SystemEntry,
  ToolResultEntry,
  UserEntry,
} from "pi-durable:internal/src/durable/entries";
export {
  ConversationBusy,
  ReadAfterWrite,
  ReadOnlySession,
  SessionFailed,
  StorageRequestError,
} from "pi-durable:bridge";
export {
  AgentDoc,
  configure,
  DEFAULT_COMPACTION_POLICY,
  DEFAULT_PROGRESS_POLICY,
  DEFAULT_RESTART_LIMIT,
  DEFAULT_RETRY_POLICY,
  CompactionTask,
  defineExtension,
  defineTool,
  hook,
  section,
  wrapSection,
  wrapTool,
  watchEvents,
  GenerationTask,
  DEFAULT_HARNESS_IDENTITY,
  Harness,
  InboxDoc,
  LiveDoc,
  ProviderDoc,
  createRegistry,
  DURABLE_TELEMETRY_SCHEMA,
  NestedCallDoc,
  ToolTask,
  UsageDoc,
} from "pi-durable:internal/harness";
export { createSession } from "pi-durable:internal/src/durable/session/session";
export { MemoryStorage } from "@earendil-works/pi-durable/storage/memory";
export { defineTask } from "pi-durable:internal/src/durable/tasks";
export { ROOT_CONVERSATION_ID } from "pi-durable:internal/src/durable/types";
