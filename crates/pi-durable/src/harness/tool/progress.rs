use super::{api::Reported, *};
use crate::{
    harness::{
        json::assign_json_ops,
        live::{self, LIVE_DOC},
    },
    wire, *,
};
use parking_lot::Mutex;
use pi_chord::output::{ProgressClock, ProgressWithError, utf8_byte_length};
use std::sync::Arc;

struct Clock {
    runtime: ToolRuntime,
    last: Mutex<f64>,
}
impl ProgressClock for Clock {
    fn now(&self) -> f64 {
        if let Ok(now) = self.runtime.now() {
            *self.last.lock() = now;
        }
        *self.last.lock()
    }
    fn set_timer(&self, callback: Box<dyn FnOnce() + Send>, delay_ms: f64) -> DisarmTimer {
        self.runtime.set_timer(callback, delay_ms)
    }
}
#[derive(Clone, Default)]
struct Written {
    text: JsonString,
    details: Option<Arc<JsonValue>>,
    diagnostics: usize,
}
pub(crate) fn publish(
    runtime: ToolRuntime,
    reported: Arc<Mutex<Reported>>,
    enabled: bool,
    context: Context,
) -> Result<ProgressWithError<Error>> {
    let interval = runtime.settings()?.progress.output_interval_ms;
    let clock = Arc::new(Clock {
        runtime: runtime.clone(),
        last: Mutex::new(0.0),
    });
    let written = Arc::new(Mutex::new(Written::default()));
    let reporter = runtime.clone();
    Ok(ProgressWithError::with_clock(
        move || -> crate::storage::StorageFuture<usize> {
            if !enabled {
                return crate::storage::ready(Ok(0));
            }
            // The callback captures synchronously, before the progress future is polled.
            let (snapshot, current, added, details_changed, bytes) = {
                let mut reported = reported.lock();
                let snapshot = reported.output.snapshot();
                let previous = written.lock();
                let current = Written {
                    text: snapshot.text.clone(),
                    details: reported.details.clone(),
                    diagnostics: reported.diagnostics.len(),
                };
                let added =
                    reported.diagnostics[previous.diagnostics..current.diagnostics].to_vec();
                let details_changed = match (&current.details, &previous.details) {
                    (Some(a), Some(b)) => !Arc::ptr_eq(a, b),
                    (None, None) => false,
                    _ => true,
                };
                let mut bytes = 0;
                if snapshot.text != previous.text {
                    let shared = if snapshot.text.units().starts_with(previous.text.units()) {
                        previous.text.units().len()
                    } else {
                        overlap(&previous.text, &snapshot.text)
                    };
                    bytes += utf8_byte_length(&JsonString::from_utf16(
                        snapshot.text.units()[shared..].to_vec(),
                    ));
                }
                if details_changed {
                    bytes +=
                        utf8_byte_length(&crate::wire::encode(&current.details).to_string().into());
                }
                if !added.is_empty() {
                    bytes += utf8_byte_length(&crate::wire::encode(&added).to_string().into());
                }
                (snapshot, current, added, details_changed, bytes)
            };
            let runtime = runtime.clone();
            let context = context.clone();
            let written = written.clone();
            Box::pin(async move {
                let conversation = runtime.conversation_id();
                let task = runtime.task_id().untyped();
                let captured = current.clone();
                runtime
                    .commit(
                        move |tx, _| async move {
                            let live = tx.doc(&LIVE_DOC, conversation).await?;
                            let selected = live.with_json(|raw| {
                                live::locate_slot(raw, task)
                                    .map(|at| (at, live::slot_json(raw, at).clone()))
                            })?;
                            if let Some((at, slot)) = selected {
                                use pi_chord::delta::{Op, Seg};
                                let (field, index) = (at.field, at.index);
                                let slot = &slot;
                                let path = |field: &str| vec![Seg::from(field), Seg::Index(index)];
                                let mut ops = Vec::new();
                                let mut assign = |key: &str, value: JsonValue| {
                                    let mut child = path(field);
                                    child.push(key.into());
                                    ops.extend(assign_json_ops(
                                        wire::get(slot, key),
                                        &value,
                                        &child,
                                    ));
                                };
                                if slot["output"].as_string().cloned().unwrap_or_default()
                                    != snapshot.text
                                {
                                    assign("output", wire::encode(&snapshot.text));
                                }
                                if snapshot.dropped_bytes > 0 {
                                    assign("droppedBytes", wire::encode(&snapshot.dropped_bytes));
                                }
                                if snapshot.dropped_lines > 0 {
                                    assign("droppedLines", wire::encode(&snapshot.dropped_lines));
                                }
                                if details_changed && let Some(details) = captured.details {
                                    assign("details", (*details).clone());
                                }
                                if !added.is_empty() {
                                    let mut diagnostics = path(field);
                                    diagnostics.push("diagnostics".into());
                                    let prior = wire::get(slot, "diagnostics")
                                        .and_then(JsonValue::as_array);
                                    if prior.is_none() {
                                        ops.push(Op::Set(
                                            diagnostics.clone(),
                                            wire::encode(&added),
                                        ));
                                    } else {
                                        let size = prior.map_or(0, Vec::len);
                                        for (offset, diagnostic) in added.iter().enumerate() {
                                            ops.push(Op::Splice(
                                                diagnostics.clone(),
                                                size + offset,
                                                0,
                                                vec![wire::encode(diagnostic)],
                                            ));
                                        }
                                    }
                                }
                                live.apply_ops(&ops)?;
                            }
                            Ok(None)
                        },
                        &context,
                    )
                    .await?;
                *written.lock() = current;
                Ok(bytes)
            })
        },
        move |error| {
            if !reporter.signal().aborted() {
                let _ = reporter.report(error);
            }
        },
        interval,
        clock,
    ))
}

fn overlap(previous: &JsonString, next: &JsonString) -> usize {
    pi_chord::delta::overlap_units(previous.units(), next.units(), 65_536, 64, 8)
}
