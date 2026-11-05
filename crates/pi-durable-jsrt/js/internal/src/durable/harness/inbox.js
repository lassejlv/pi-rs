// packages/durable/src/harness/inbox.ts: `InboxDoc`.
import { defineDoc } from "pi-durable:internal/src/durable/documents";

export const InboxDoc = defineDoc({
  kind: "pi.inbox",
  version: 1,
  scope: "conversation",
  history: "latest",
  fork: "initial",
  initial: () => ({ items: [] }),
  checkpointWhen: (value) => value.items.length === 0,
});
