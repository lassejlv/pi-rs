//! Stock tests supply the recording adapter; Rust owns instrumentation decisions.
use super::*;
use pi_telemetry::*;

struct HostTelemetry {
    hub: std::sync::Weak<Hub>,
    id: u64,
    metrics: Option<u64>,
    propagation: Option<u64>,
}
struct HostMetrics {
    hub: std::sync::Weak<Hub>,
    id: u64,
}
struct HostPropagation {
    hub: std::sync::Weak<Hub>,
    id: u64,
}
struct HostInstrument {
    hub: std::sync::Weak<Hub>,
    id: u64,
}
fn invoke(hub: &std::sync::Weak<Hub>, method: &str, value: JsonValue) -> JsonValue {
    hub.upgrade()
        .expect("live telemetry test host")
        .sync(method, value)
        .expect("telemetry adapter call")
}
pub(super) fn context(hub: &Arc<Hub>, descriptor: &JsonValue) -> ContextRef {
    if descriptor.is_null() {
        return noop_telemetry_context();
    }
    host(hub, descriptor)
}
fn host(hub: &Arc<Hub>, descriptor: &JsonValue) -> Arc<HostTelemetry> {
    Arc::new(HostTelemetry {
        hub: Arc::downgrade(hub),
        id: field(descriptor, "id").unwrap(),
        metrics: field(descriptor, "metrics").unwrap(),
        propagation: field(descriptor, "propagation").unwrap(),
    })
}
pub(super) fn id(context: &dyn TelemetryContext) -> Option<u64> {
    (context as &dyn std::any::Any)
        .downcast_ref::<HostTelemetry>()
        .map(|context| context.id)
}
impl TelemetryContext for HostTelemetry {
    fn update_attributes(&self, attributes: SpanAttributes) {
        self.set_attributes(attributes);
    }
    fn record_event(&self, name: &str, attributes: SpanAttributes) {
        self.add_event(name, attributes);
    }
    fn metrics(&self) -> Option<MetricsRef> {
        self.metrics.map(|id| {
            Arc::new(HostMetrics {
                hub: self.hub.clone(),
                id,
            }) as MetricsRef
        })
    }
    fn propagation(&self) -> Option<PropagationRef> {
        self.propagation.map(|id| {
            Arc::new(HostPropagation {
                hub: self.hub.clone(),
                id,
            }) as PropagationRef
        })
    }
    fn begin_span(&self, options: SpanOptions) -> SpanRef {
        let links = options
            .links
            .iter()
            .map(|link| {
                object([
                    ("context", encode(&id(link.context.as_ref()))),
                    ("attributes", encode(&link.attributes)),
                ])
            })
            .collect::<Vec<_>>();
        let value = invoke(
            &self.hub,
            "telemetry.begin",
            object([
                ("context", encode(&self.id)),
                ("name", encode(&options.name)),
                ("attributes", encode(&options.attributes)),
                ("links", encode(&links)),
            ]),
        );
        host(&self.hub.upgrade().unwrap(), &value)
    }
}
impl TelemetrySpan for HostTelemetry {
    fn add_link(&self, link: SpanLink) {
        invoke(
            &self.hub,
            "telemetry.link",
            object([
                ("span", encode(&self.id)),
                ("context", encode(&id(link.context.as_ref()))),
                ("attributes", encode(&link.attributes)),
            ]),
        );
    }
    fn add_event(&self, name: &str, attributes: SpanAttributes) {
        invoke(
            &self.hub,
            "telemetry.event",
            object([
                ("span", encode(&self.id)),
                ("name", encode(&name)),
                ("attributes", encode(&attributes)),
            ]),
        );
    }
    fn set_attributes(&self, attributes: SpanAttributes) {
        invoke(
            &self.hub,
            "telemetry.attributes",
            object([
                ("span", encode(&self.id)),
                ("attributes", encode(&attributes)),
            ]),
        );
    }
    fn set_status(&self, status: SpanStatus) {
        invoke(
            &self.hub,
            "telemetry.status",
            object([("span", encode(&self.id)), ("status", encode(&status))]),
        );
    }
    fn end(&self, outcome: SpanOutcome) {
        invoke(
            &self.hub,
            "telemetry.end",
            object([
                ("span", encode(&self.id)),
                (
                    "error",
                    encode(&match outcome {
                        SpanOutcome::Ok => None,
                        SpanOutcome::Failed(error) => error,
                    }),
                ),
            ]),
        );
    }
}
impl TelemetryPropagation for HostPropagation {
    fn inject(&self, context: &dyn TelemetryContext) -> Option<TelemetryCarrier> {
        let context = id(context)?;
        decode(&invoke(
            &self.hub,
            "telemetry.inject",
            object([
                ("propagation", encode(&self.id)),
                ("context", encode(&context)),
            ]),
        ))
        .unwrap()
    }
    fn extract(&self, carrier: Option<&TelemetryCarrier>) -> ContextRef {
        let value = invoke(
            &self.hub,
            "telemetry.extract",
            object([
                ("propagation", encode(&self.id)),
                ("carrier", encode(&carrier)),
            ]),
        );
        context(&self.hub.upgrade().unwrap(), &value)
    }
}
impl HostMetrics {
    fn instrument(
        &self,
        kind: &str,
        name: &str,
        options: InstrumentOptions,
    ) -> Arc<HostInstrument> {
        let options = object([
            ("description", encode(&options.description)),
            ("unit", encode(&options.unit)),
            ("boundaries", encode(&options.boundaries)),
        ]);
        let id = decode(&invoke(
            &self.hub,
            "telemetry.instrument",
            object([
                ("metrics", encode(&self.id)),
                ("kind", encode(&kind)),
                ("name", encode(&name)),
                ("options", options),
            ]),
        ))
        .unwrap();
        Arc::new(HostInstrument {
            hub: self.hub.clone(),
            id,
        })
    }
}
impl TelemetryMetrics for HostMetrics {
    fn counter(&self, name: &str, options: InstrumentOptions) -> CounterRef {
        self.instrument("counter", name, options)
    }
    fn up_down_counter(&self, name: &str, options: InstrumentOptions) -> UpDownCounterRef {
        self.instrument("upDownCounter", name, options)
    }
    fn histogram(&self, name: &str, options: InstrumentOptions) -> HistogramRef {
        self.instrument("histogram", name, options)
    }
}
impl HostInstrument {
    fn add(&self, value: f64, attributes: SpanAttributes) {
        invoke(
            &self.hub,
            "telemetry.add",
            object([
                ("instrument", encode(&self.id)),
                ("value", encode(&value)),
                ("attributes", encode(&attributes)),
            ]),
        );
    }
}
impl TelemetryCounter for HostInstrument {
    fn add(&self, value: f64, attributes: SpanAttributes) {
        self.add(value, attributes);
    }
}
impl TelemetryUpDownCounter for HostInstrument {
    fn add(&self, value: f64, attributes: SpanAttributes) {
        self.add(value, attributes);
    }
}
impl TelemetryHistogram for HostInstrument {
    fn record(
        &self,
        value: f64,
        attributes: SpanAttributes,
        context: Option<&dyn TelemetryContext>,
    ) {
        invoke(
            &self.hub,
            "telemetry.record",
            object([
                ("instrument", encode(&self.id)),
                ("value", encode(&value)),
                ("attributes", encode(&attributes)),
                ("context", encode(&context.and_then(id))),
            ]),
        );
    }
}
