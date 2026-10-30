// packages/durable/src/entries.ts.

/** Define a typed entry kind whose `is()` guard narrows by `EntryRecord.kind`. */
export function defineEntry(kind) {
  if (typeof kind !== "string" || kind.length === 0) throw new TypeError("Entry kind must be a non-empty string");
  return {
    kind,
    is: (entry) => entry !== undefined && entry.kind === kind,
  };
}

export const UserEntry = defineEntry("pi.user");
export const AssistantEntry = defineEntry("pi.assistant");
export const SystemEntry = defineEntry("pi.system");
export const ToolResultEntry = defineEntry("pi.tool-result");
export const ResetEntry = defineEntry("pi.reset");
export const CompactionEntry = defineEntry("pi.compaction");
