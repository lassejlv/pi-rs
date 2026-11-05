// packages/durable/src/harness/provider.ts: `ProviderDoc`.
import { uuidv7 } from "@earendil-works/pi-ai";
import { defineDoc } from "pi-durable:internal/src/durable/documents";

/** Built-in provider state; every fork starts with a fresh identity instead of copying its parent. */
export const ProviderDoc = defineDoc({
  kind: "pi.provider",
  version: 1,
  scope: "conversation",
  history: "latest",
  fork: "initial",
  initial: () => ({ sessionId: uuidv7() }),
  checkpointWhen: () => true,
});
