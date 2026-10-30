// @earendil-works/pi-telemetry (packages/telemetry/src/index.ts) for the embedded packages, so
// they need nothing from node_modules. The no-op capabilities and the schema/typing helpers are
// upstream's (they only shape JS objects); `InMemoryTelemetryContext` records natively in
// pi_telemetry::InMemoryTelemetryContext (src/dispatch/telemetry.rs `telemetry.*`): spans,
// carriers, links, events, statuses, measurements and the store queries. `startSpan` keeps
// upstream's callback scoping: the span is admitted and the callback called synchronously, and the
// span settles when the callback's result settles.
import { call } from "pi-durable:bridge";

// --- noop.ts ---

async function startNoopSpan(_options, callback) {
  return callback(noopTelemetrySpan);
}

const noopCounter = Object.freeze({ add: () => {} });
const noopUpDownCounter = Object.freeze({ add: () => {} });
const noopHistogram = Object.freeze({ record: () => {} });

export const NOOP_TELEMETRY_METRICS = Object.freeze({
  counter: () => noopCounter,
  upDownCounter: () => noopUpDownCounter,
  histogram: () => noopHistogram,
});

export const NOOP_TELEMETRY_PROPAGATION = Object.freeze({
  inject: () => undefined,
  extract: () => NOOP_TELEMETRY_CONTEXT,
});

const noopTelemetrySpan = {
  metrics: NOOP_TELEMETRY_METRICS,
  propagation: NOOP_TELEMETRY_PROPAGATION,
  startSpan: startNoopSpan,
  addLink: () => {},
  addEvent: () => {},
  setAttributes: () => {},
  setStatus: () => {},
};
Object.freeze(noopTelemetrySpan);

export const NOOP_TELEMETRY_CONTEXT = noopTelemetrySpan;

// --- metrics.ts / index.ts helpers ---

export function createTypedMetrics(metrics, schemas) {
  const definitions = new Map();
  for (const schema of schemas) {
    for (const [name, definition] of Object.entries(schema.metrics ?? {})) definitions.set(name, definition);
  }
  return {
    counter: (name) => metrics.counter(name, definitions.get(name)),
    upDownCounter: (name) => metrics.upDownCounter(name, definitions.get(name)),
    histogram: (name) => metrics.histogram(name, definitions.get(name)),
  };
}

export function defineTelemetrySchema(schema) {
  return schema;
}

function bindTypedSpanStarter(telemetryContext) {
  return (name, attributes, callback) =>
    telemetryContext.startSpan({ name, attributes }, (span) => callback(span, bindTypedSpanStarter(span)));
}

/** Bind an explicit parent context to the span vocabulary of one or more schemas (no runtime validation). */
export function createTypedSpanStarter(telemetryContext, _schemas) {
  return bindTypedSpanStarter(telemetryContext);
}

// --- memory.ts over the native recorder ---

/** The native span or extracted parent each JS context object stands for (any recorder). */
const natives = new WeakMap();

const clocks = () => [Date.now(), performance.now()];

function wireOptions(options) {
  const links = [];
  for (const link of options?.links ?? []) {
    const native = natives.get(link.context);
    if (native !== undefined) links.push({ context: native, attributes: link.attributes });
  }
  return { name: String(options?.name), attributes: options?.attributes, links };
}

function startNativeSpan(recorder, owner, parent, options, callback) {
  const native = call("telemetry.begin", recorder, ...clocks(), parent, wireOptions(options));
  const span = {
    metrics: owner,
    propagation: owner,
    startSpan: (childOptions, childCallback) => startNativeSpan(recorder, owner, native, childOptions, childCallback),
    addLink(link) {
      const context = natives.get(link?.context);
      if (context !== undefined) call("telemetry.update", recorder, ...clocks(), native, "link", { context, attributes: link.attributes });
    },
    addEvent(name, attributes) {
      call("telemetry.update", recorder, ...clocks(), native, "event", String(name), attributes);
    },
    setAttributes(attributes) {
      call("telemetry.update", recorder, ...clocks(), native, "attributes", attributes);
    },
    setStatus(status) {
      call("telemetry.update", recorder, ...clocks(), native, "status", {
        status: status?.status,
        ...(status?.error === undefined ? {} : { error: { name: status.error.name, message: status.error.message } }),
      });
    },
  };
  natives.set(span, native);
  const settle = (failed, error) => call("telemetry.end", recorder, ...clocks(), native, failed, error);
  return (async () => {
    try {
      const result = await callback(span);
      settle(false);
      return result;
    } catch (error) {
      settle(true, error instanceof Error ? { name: error.name, message: error.message } : undefined);
      throw error;
    }
  })();
}

function instrument(recorder, kind, name, options) {
  const native = call("telemetry.instrument", recorder, kind, String(name), {
    ...(options?.description === undefined ? {} : { description: options.description }),
    ...(options?.unit === undefined ? {} : { unit: options.unit }),
    ...(kind === "histogram" && options?.boundaries !== undefined ? { boundaries: [...options.boundaries] } : {}),
  });
  return (value, attributes, context) =>
    call("telemetry.record", native, ...clocks(), value, attributes, context === undefined ? undefined : natives.get(context));
}

// Upstream's recorder state is TypeScript-private (a plain property), so wrappers such as
// `Object.create(telemetry)` keep working: the native recorder is held under a module symbol.
const RECORDER = Symbol("pi-telemetry recorder");

export class InMemoryTelemetryContext {
  metrics = this;
  propagation = this;

  constructor() {
    Object.defineProperty(this, RECORDER, { value: call("telemetry.memory", ...clocks()) });
  }

  startSpan(options, callback) {
    return startNativeSpan(this[RECORDER], this, undefined, options, callback);
  }

  inject(context) {
    const native = natives.get(context);
    return native === undefined ? undefined : call("telemetry.inject", this[RECORDER], native);
  }

  extract(carrier) {
    const scope = carrier?.["pi-memory-scope"];
    const span = carrier?.["pi-memory-span"];
    const traceId = carrier?.["pi-memory-trace"];
    if (typeof scope !== "string" || typeof span !== "string" || typeof traceId !== "string") return this;
    const recorder = this[RECORDER];
    const native = call("telemetry.extract", recorder, { "pi-memory-scope": scope, "pi-memory-span": span, "pi-memory-trace": traceId });
    const parent = {
      metrics: this,
      propagation: this,
      startSpan: (options, callback) => startNativeSpan(recorder, this, native, options, callback),
    };
    natives.set(parent, native);
    return parent;
  }

  counter(name, options) {
    return { add: instrument(this[RECORDER], "counter", name, options) };
  }

  upDownCounter(name, options) {
    return { add: instrument(this[RECORDER], "upDownCounter", name, options) };
  }

  histogram(name, options) {
    return { record: instrument(this[RECORDER], "histogram", name, options) };
  }

  async getTrace(traceId) {
    return call("telemetry.getTrace", this[RECORDER], String(traceId));
  }

  async findTraces(query = {}) {
    return call("telemetry.findTraces", this[RECORDER], query);
  }

  async findMetrics(query) {
    return call("telemetry.findMetrics", this[RECORDER], query);
  }

  getMetrics() {
    return call("telemetry.metrics", this[RECORDER]);
  }

  getSpans() {
    return call("telemetry.spans", this[RECORDER]);
  }
}
