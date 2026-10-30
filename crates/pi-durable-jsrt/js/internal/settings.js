// pi-durable:internal/settings — Harness settings on the wire (port of
// scripts/stock/durable-settings-wire.mjs): non-finite and negative-zero numbers cross as strings.

/** The wire request for `settings`, and the extensions it names (kept as JS objects). */
export function captureSettings(settings) {
  const extensions = settings?.extensions;
  const input = {
    extensions: extensions?.map((extension) => extension.name),
    stream: settings?.stream,
    retry: settings?.retry,
    compaction: settings?.compaction,
    progress: { partialIntervalMs: settings?.progress?.partialIntervalMs, outputIntervalMs: settings?.progress?.outputIntervalMs },
    toolExecution: settings?.toolExecution,
    steeringMode: settings?.steeringMode,
    followUpMode: settings?.followUpMode,
    contextRetentionMs: settings?.contextRetentionMs,
    documentRetentionMs: settings?.documentRetentionMs,
    afterFailedRun: settings?.afterFailedRun,
    restartLimit: settings?.restartLimit,
  };
  const runtimeNumbers = {};
  for (const key of ["contextRetentionMs", "documentRetentionMs", "restartLimit"]) {
    const value = input[key];
    if (typeof value === "number" && (!Number.isFinite(value) || Object.is(value, -0))) {
      runtimeNumbers[key] = Object.is(value, -0) ? "-0" : String(value);
      input[key] = null;
    }
  }
  for (const key of ["partialIntervalMs", "outputIntervalMs"]) {
    const value = input.progress[key];
    if (typeof value === "number" && (!Number.isFinite(value) || Object.is(value, -0))) {
      runtimeNumbers[`progress.${key}`] = Object.is(value, -0) ? "-0" : String(value);
      input.progress[key] = null;
    }
  }
  return { request: { settings: input, runtimeNumbers }, extensions };
}

export function settingsRequest(settings) {
  return captureSettings(settings).request;
}

/** Resolved settings from the wire. */
export function hydrateSettings(value) {
  const number = (v) => (v && typeof v === "object" && Object.hasOwn(v, "$nativeNumber") ? Number(v.$nativeNumber) : v);
  for (const key of ["contextRetentionMs", "documentRetentionMs", "restartLimit"]) value[key] = number(value[key]);
  for (const key of ["partialIntervalMs", "outputIntervalMs"]) value.progress[key] = number(value.progress[key]);
  return value;
}
