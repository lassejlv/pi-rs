// packages/durable/src/documents.ts: document definitions. A token keeps the caller's definition
// object (its `initial`, `migrate` and `checkpointWhen` callbacks stay JS); the Session hands the
// native kernel a description of it (see session/session.js).

function validateDefinition(definition) {
  if (!Number.isSafeInteger(definition.version) || definition.version < 1) {
    throw new TypeError(`Document ${definition.kind} version must be a positive integer`);
  }
}

/** Define a singleton document. */
export function defineDoc(definition) {
  validateDefinition(definition);
  return { definition };
}

/** Define a document family. */
export function defineDocFamily(definition) {
  validateDefinition(definition);
  return { definition };
}

function ownerId(value, definition) {
  if (typeof value !== "number" || !Number.isSafeInteger(value)) {
    throw new TypeError(`Document ${definition.kind} requires a ${definition.scope} ID`);
  }
  return value;
}

/** Resolve an overloaded argument list and return the index after the owner and family key. */
export function resolveAddress(definition, args) {
  let index = 0;
  let scope;
  switch (definition.scope) {
    case "session":
      scope = { kind: "session" };
      break;
    case "conversation":
      scope = { kind: "conversation", conversationId: ownerId(args[index++], definition) };
      break;
    case "task":
      scope = { kind: "task", taskId: ownerId(args[index++], definition) };
      break;
  }
  const key = definition.family === true ? args[index++] : undefined;
  const address = key === undefined ? { kind: definition.kind, scope } : { kind: definition.kind, scope, key };
  return { address, id: addressId(address), nextArgument: index };
}

/** Stable string identity of one logical address. */
export function addressId(address) {
  const owner =
    address.scope.kind === "session"
      ? null
      : address.scope.kind === "conversation"
        ? address.scope.conversationId
        : address.scope.taskId;
  return JSON.stringify([address.kind, address.scope.kind, owner, address.key ?? null]);
}
