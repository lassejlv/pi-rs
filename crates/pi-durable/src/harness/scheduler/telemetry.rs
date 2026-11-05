//! A phase owns its handler and decision commit, including a commit waiting on storage.
use super::*;
use pi_chord::context::with_telemetry;
use pi_telemetry::{
    SpanError, SpanLink, SpanOptions, SpanOutcome, SpanRef, SpanStatus, TelemetryText,
};
use web_time::Instant;

pub(super) struct Phase {
    scheduler: TaskScheduler,
    invocation: Arc<Invocation>,
    previous: Context,
    span: SpanRef,
    phase: JsonString,
    task: JsonString,
    started: Instant,
}

fn carrier(value: &JsonObject) -> Option<pi_telemetry::TelemetryCarrier> {
    value
        .iter()
        .map(|(key, value)| {
            Some((
                TelemetryText::from(key),
                TelemetryText::from(value.as_string()?),
            ))
        })
        .collect()
}

impl Phase {
    pub(super) fn begin(
        scheduler: &TaskScheduler,
        invocation: Arc<Invocation>,
        task: &TaskRecord,
    ) -> Self {
        let root = &scheduler.inner.options.telemetry;
        let parent = task
            .info
            .trace
            .as_ref()
            .and_then(|trace| Some(root.propagation()?.extract(carrier(trace).as_ref())))
            .unwrap_or_else(|| root.clone());
        let checkpoint = task.state.checkpoint().expect("running phase checkpoint");
        let phase = crate::wire::get(checkpoint, "phase")
            .and_then(JsonValue::as_string)
            .cloned()
            .unwrap_or_default();
        let resumed = scheduler.inner.state.lock().resumed.remove(&task.id());
        let mut attributes = pi_telemetry::attributes! {
            "task.id" => task.id().get() as f64,
            "task.name" => task.info.kind.clone(),
            "phase" => phase.clone(),
            "kind" => if invocation.mode == InvocationMode::Run { "run" } else { "abort" },
            "resumed" => resumed
        };
        if let Some(attempt) = crate::wire::get(checkpoint, "attempt").and_then(JsonValue::as_f64) {
            attributes.insert("attempt".into(), attempt.into());
        }
        let span = parent.begin_span(SpanOptions::new("pi.durable.phase").attributes(attributes));
        if let Some(propagation) = span.propagation() {
            for link in task.info.links.iter().flatten() {
                span.add_link(SpanLink::new(propagation.extract(carrier(link).as_ref())));
            }
        }
        let previous = {
            let mut context = invocation.context.lock();
            let previous = context.clone();
            *context = with_telemetry(span.clone(), &previous);
            previous
        };
        Self {
            scheduler: scheduler.clone(),
            invocation,
            previous,
            span,
            phase,
            task: task.info.kind.clone(),
            started: Instant::now(),
        }
    }
}

impl Drop for Phase {
    fn drop(&mut self) {
        *self.invocation.context.lock() = self.previous.clone();
        let record = self.invocation.record.lock().clone();
        let fault = self.invocation.fault.lock().clone();
        let terminal = fault.as_ref().or(match &record.state {
            TaskState::Terminal { outcome } | TaskState::Completing { outcome } => Some(outcome),
            _ => None,
        });
        let outcome: JsonString = match terminal {
            Some(TaskOutcome::Completed { .. }) => "done".into(),
            Some(TaskOutcome::Aborted { .. }) => "aborted".into(),
            Some(_) => "faulted".into(),
            None => match &record.state {
                TaskState::Waiting { .. } => "waiting".into(),
                TaskState::Sleeping { .. } => "sleeping".into(),
                _ if self.invocation.ended.load(Ordering::Acquire)
                    || self.scheduler.inner.state.lock().closing =>
                {
                    "aborted".into()
                }
                _ => {
                    let next = record
                        .state
                        .checkpoint()
                        .and_then(|checkpoint| crate::wire::get(checkpoint, "phase"))
                        .and_then(JsonValue::as_string);
                    let mut units = "next=".encode_utf16().collect::<Vec<_>>();
                    units.extend_from_slice(next.map_or(&[], JsonString::units));
                    JsonString::from_utf16(units)
                }
            },
        };
        self.span
            .set_attributes(pi_telemetry::attributes! { "outcome" => outcome.clone() });
        let error = match terminal {
            Some(TaskOutcome::Faulted { error }) => Some(("task_faulted", &error.message)),
            Some(TaskOutcome::Failed { error, .. }) => Some(("task_failed", &error.message)),
            Some(TaskOutcome::Orphaned { reason }) => Some(("task_orphaned", reason)),
            _ => None,
        };
        if let Some((name, message)) = error {
            self.span.set_status(SpanStatus::error(SpanError::new(
                name,
                TelemetryText::from(message).prefix(500),
            )));
        } else if outcome == JsonString::from("aborted") {
            self.span.set_status(SpanStatus::Ok);
        }
        self.scheduler.inner.duration_metric.record(
            self.started.elapsed().as_secs_f64() * 1000.0,
            pi_telemetry::attributes! { "task" => self.task.clone(), "phase" => self.phase.clone(), "outcome" => outcome },
            Some(self.span.as_ref()),
        );
        self.span.end(SpanOutcome::Ok);
    }
}
