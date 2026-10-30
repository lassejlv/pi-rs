// packages/durable/src/harness/agent.ts: the agent document, policies and `configure` (glue over
// `tx`), with settings and agents resolved by the native registry (pi-durable:internal/registry).
import { copyJson } from "@earendil-works/chord";
import { defineDoc } from "pi-durable:internal/src/durable/documents";
export { agentHooks, resolveAgent, resolveSettings, sectionExtension, toolExtension } from "pi-durable:internal/registry";

export const DEFAULT_RETRY_POLICY = {
  enabled: true,
  maxRetries: 3,
  baseDelayMs: 2000,
  maxAgentDelayMs: 60000,
};

export const DEFAULT_COMPACTION_POLICY = {
  enabled: true,
  reserveTokens: 16384,
  keepRecentTokens: 20000,
  backgroundTokens: 32768,
};

export const DEFAULT_PROGRESS_POLICY = {
  partialIntervalMs: 100,
  outputIntervalMs: 100,
};

/** The reserved section key of the agent's `instructions`. */
export const INSTRUCTIONS_KEY = "instructions";

/** Built-in agent document; rewindable so forks start from the agent at their fork entry. */
export const AgentDoc = defineDoc({
  kind: "pi.agent",
  version: 1,
  scope: "conversation",
  history: "rewindable",
  fork: "asOf",
  initial: () => ({}),
  checkpointWhen: () => true,
});

/** Default `settings.restartLimit`. */
export const DEFAULT_RESTART_LIMIT = 5;

/** Apply one change to `pi.agent`: a given field replaces the stored one, `null` clears it, `undefined` changes nothing. */
export async function configure(tx, conversationId, change) {
  const state = await tx.doc(AgentDoc, conversationId);
  applyChange(state, change);
}

/** `addTools` of a tool round, applied to `tools` and `modelTools`. */
export async function addTools(tx, conversationId, added) {
  const state = await tx.doc(AgentDoc, conversationId);
  const tools = state.tools;
  if (Array.isArray(tools)) {
    for (const name of added) if (!tools.includes(name)) tools.push(name);
  } else if (tools?.remove.some((name) => added.includes(name))) {
    state.tools = { remove: tools.remove.filter((name) => !added.includes(name)) };
  }
  const modelTools = state.modelTools;
  if (Array.isArray(modelTools)) {
    for (const name of added) if (!modelTools.includes(name)) modelTools.push(name);
    return;
  }
  // Assigned whole; a document write of an equal value records no change.
  const remove = (modelTools?.remove ?? []).filter((name) => !added.includes(name));
  const add = [...(modelTools?.add ?? [])];
  for (const name of added) if (!add.includes(name)) add.push(name);
  state.modelTools = { ...(remove.length === 0 ? {} : { remove }), add };
}

function applyChange(state, change) {
  const set = (key, value) => {
    if (value === undefined) return;
    if (value === null) delete state[key];
    else state[key] = value;
  };
  set("model", change.model === undefined || change.model === null ? change.model : { ...change.model });
  set("thinkingLevel", change.thinkingLevel);
  const extensions = change.extensions;
  set(
    "extensions",
    extensions === undefined || extensions === null
      ? extensions
      : Array.isArray(extensions)
        ? names(extensions)
        : {
            ...(extensions.add === undefined ? {} : { add: names(extensions.add) }),
            ...(extensions.remove === undefined ? {} : { remove: names(extensions.remove) }),
          },
  );
  const tools = change.tools;
  set("tools", tools === undefined || tools === null ? tools : Array.isArray(tools) ? names(tools) : { remove: names(tools.remove) });
  const modelTools = change.modelTools;
  set(
    "modelTools",
    modelTools === undefined || modelTools === null
      ? modelTools
      : Array.isArray(modelTools)
        ? names(modelTools)
        : {
            ...(modelTools.remove === undefined ? {} : { remove: names(modelTools.remove) }),
            ...(modelTools.add === undefined ? {} : { add: names(modelTools.add) }),
          },
  );
  set("instructions", change.instructions);
  set("cwd", change.cwd);
}

function names(items) {
  return items.map((item) => item.name);
}

/** The stored agent of a conversation created or forked in `tx`. */
export async function createAgent(tx, conversation) {
  if (conversation.parent !== undefined) return;
  const agent = await tx.doc(AgentDoc, conversation.id);
  if (conversation.owner === undefined) return;
  const owner = await tx.doc(AgentDoc, conversation.owner.conversationId);
  Object.assign(agent, copyJson(owner));
}
