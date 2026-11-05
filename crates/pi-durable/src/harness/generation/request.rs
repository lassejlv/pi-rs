use super::{settle::*, types::*, *};
use crate::harness::{
    live::{LIVE_DOC, LiveGeneration},
    provider::ensure_provider_session_id,
};
use parking_lot::Mutex;

pub(super) async fn request(
    task: Running,
    runtime: GenerationRuntime,
    context: Context,
) -> Result<()> {
    let GenerationCheckpoint::Request(request) = task.checkpoint().clone() else {
        return Err(Error::message("Expected request checkpoint"));
    };
    let conversation = runtime.conversation_id();
    let attempt = request.attempt;
    runtime
        .commit(
            move |tx, _| async move {
                let live = tx.doc(&LIVE_DOC, conversation).await?;
                convert_partial(&tx, &live, conversation).await?;
                live.apply_ops(&[Op::Set(
                    vec!["generation".into()],
                    wire::encode(&LiveGeneration {
                        attempt,
                        message: None,
                        retry: None,
                        deferred: None,
                    }),
                )])?;
                Ok(None)
            },
            &context,
        )
        .await?;
    let Some(model) = resolve(&runtime, &request.model)? else {
        return fail_no_model(&runtime, Some(request.model), &context).await;
    };
    let view = runtime
        .context(conversation, Some(request.cutoff), &context)
        .await?;
    let messages = Arc::new(Mutex::new(view.messages.clone()));
    let captured = messages.clone();
    let current = runtime.clone();
    let callback_context = context.clone();
    runtime
        .hooks()
        .each(&BEFORE_REQUEST, move |hook| {
            let current = current.clone();
            let context = callback_context.clone();
            let messages = captured.clone();
            async move {
                let before = messages.lock().clone();
                if let Some(next) = hook(before, current, context).await? {
                    *messages.lock() = next;
                }
                Ok(())
            }
        })
        .await?;
    let messages = messages.lock().clone();
    let mut options = request.stream_options.clone();
    options.insert(
        "sessionId".into(),
        wire::encode(&ensure_provider_session_id(&runtime, &context).await?),
    );
    if !request.thinking_level.equals_str("off") {
        options.insert("reasoning".into(), wire::encode(&request.thinking_level));
    }
    let message = stream_response(&runtime, model, messages, options, attempt, &context).await;
    classify(
        runtime,
        ResponseRequest {
            attempt,
            compacted: request.compacted,
            model: request.model,
            cutoff: request.cutoff,
            messages: Some(view.messages),
            poll_at: None,
        },
        message?,
        context,
    )
    .await
}
pub(super) async fn retry(
    task: Running,
    runtime: GenerationRuntime,
    context: Context,
) -> Result<()> {
    let GenerationCheckpoint::Retry(backoff) = task.checkpoint().clone() else {
        return Err(Error::message("Expected retry checkpoint"));
    };
    if backoff.until > runtime.now()? {
        return runtime
            .sleep_until(backoff.until, task.checkpoint().clone(), &context)
            .await;
    }
    let conversation = runtime.conversation_id();
    let attempt = backoff.attempt + 1;
    runtime
        .commit(
            move |tx, _| async move {
                tx.doc(&LIVE_DOC, conversation).await?.apply_ops(&[Op::Set(
                    vec!["generation".into()],
                    wire::encode(&LiveGeneration {
                        attempt,
                        message: None,
                        retry: None,
                        deferred: None,
                    }),
                )])?;
                Ok(Some(Next::Running {
                    checkpoint: GenerationCheckpoint::Prepare(Preparation {
                        attempt,
                        compacted: backoff.compacted,
                        overflow: None,
                        threshold: None,
                    }),
                }))
            },
            &context,
        )
        .await
}
pub(super) async fn poll(
    task: Running,
    runtime: GenerationRuntime,
    context: Context,
) -> Result<()> {
    let GenerationCheckpoint::Poll(poll) = task.checkpoint().clone() else {
        return Err(Error::message("Expected poll checkpoint"));
    };
    if poll.poll_at > runtime.now()? {
        return runtime
            .sleep_until(poll.poll_at, task.checkpoint().clone(), &context)
            .await;
    }
    let Some(model) = resolve(&runtime, &poll.model)? else {
        return fail_no_model(&runtime, Some(poll.model), &context).await;
    };
    let message = runtime
        .model_collection()?
        .fetch_deferred(
            model,
            poll.handle.clone(),
            pi_chord::context::with_abort_signal(runtime.signal(), &context),
        )
        .await?;
    classify(
        runtime,
        ResponseRequest {
            attempt: poll.attempt,
            compacted: poll.compacted,
            model: poll.model,
            cutoff: poll.cutoff,
            messages: None,
            poll_at: Some(poll.poll_at),
        },
        message,
        context,
    )
    .await
}
async fn stream_response(
    runtime: &GenerationRuntime,
    model: crate::harness::models::ResolvedModel,
    messages: Vec<Arc<JsonValue>>,
    options: JsonObject,
    attempt: u64,
    context: &Context,
) -> Result<GenerationMessage> {
    let writer = PartialWriter::new(
        runtime.clone(),
        context.clone(),
        attempt,
        runtime.settings()?.progress.partial_interval_ms,
    );
    let result = async {
        let mut stream = runtime
            .model_collection()?
            .stream_simple(
                model,
                messages,
                options,
                pi_chord::context::with_abort_signal(runtime.signal(), context),
            )
            .await?;
        while let Some(partial) = stream.next().await? {
            if !partial.has_content()? {
                continue;
            }
            writer.mark(partial);
        }
        stream.result().await
    }
    .await;
    writer.stop().await;
    result
}
pub(super) fn retry_delay(policy: &JsonObject, attempt: u64) -> f64 {
    let delay = policy.value()["baseDelayMs"].as_f64().unwrap_or(0.0)
        * 2.0_f64.powf(attempt.saturating_sub(1) as f64);
    let safe =
        if delay.is_finite() && delay.fract() == 0.0 && delay.abs() <= 9_007_199_254_740_991.0 {
            delay
        } else {
            9_007_199_254_740_991.0
        };
    safe.min(
        policy.value()["maxAgentDelayMs"]
            .as_f64()
            .unwrap_or(60_000.0),
    )
}

struct PartialState {
    pending: Option<GenerationMessage>,
    timer: Option<DisarmTimer>,
    timer_generation: Option<u64>,
    next: u64,
    in_flight: bool,
    stopped: bool,
}
struct PartialInner {
    state: Mutex<PartialState>,
    runtime: GenerationRuntime,
    context: Context,
    attempt: u64,
    interval: f64,
    changed: tokio::sync::watch::Sender<u64>,
}
struct PartialWriter(Arc<PartialInner>);
impl PartialWriter {
    fn new(runtime: GenerationRuntime, context: Context, attempt: u64, interval: f64) -> Self {
        let (changed, _) = tokio::sync::watch::channel(0);
        Self(Arc::new(PartialInner {
            state: Mutex::new(PartialState {
                pending: None,
                timer: None,
                timer_generation: None,
                next: 0,
                in_flight: false,
                stopped: false,
            }),
            runtime,
            context,
            attempt,
            interval,
            changed,
        }))
    }
    fn mark(&self, message: GenerationMessage) {
        self.0.state.lock().pending = Some(message);
        Self::schedule(&self.0);
    }
    fn schedule(inner: &Arc<PartialInner>) {
        let generation = {
            let mut state = inner.state.lock();
            if state.stopped
                || state.in_flight
                || state.timer_generation.is_some()
                || state.pending.is_none()
            {
                return;
            }
            let generation = state.next;
            state.next += 1;
            state.timer_generation = Some(generation);
            generation
        };
        let weak = Arc::downgrade(inner);
        let cancel = inner.runtime.set_timer(
            move || {
                if let Some(inner) = weak.upgrade() {
                    Self::flush(inner, generation);
                }
            },
            inner.interval,
        );
        let unused = {
            let mut state = inner.state.lock();
            if state.timer_generation == Some(generation) && !state.stopped {
                state.timer = Some(cancel);
                None
            } else {
                Some(cancel)
            }
        };
        if let Some(cancel) = unused {
            cancel();
        }
    }
    fn flush(inner: Arc<PartialInner>, generation: u64) {
        let partial = {
            let mut state = inner.state.lock();
            if state.timer_generation != Some(generation) {
                return;
            }
            state.timer_generation = None;
            state.timer.take();
            if state.stopped {
                return;
            }
            let Some(partial) = state.pending.take() else {
                return;
            };
            state.in_flight = true;
            partial
        };
        let conversation = inner.runtime.conversation_id();
        let attempt = inner.attempt;
        // Admit while the timer callback owns its place on the Session queue.
        let operation = match partial.copy_json() {
            Err(error) => crate::storage::ready(Err(error)),
            Ok(partial) => inner.runtime.commit(
                move |tx, _| async move {
                    let live = tx.doc(&LIVE_DOC, conversation).await?;
                    let value = live.with_json(JsonValue::clone)?;
                    let mut ops = Vec::new();
                    if value["generation"].is_null() {
                        ops.push(Op::Set(
                            vec!["generation".into()],
                            wire::encode(&LiveGeneration {
                                attempt,
                                message: None,
                                retry: None,
                                deferred: None,
                            }),
                        ));
                    }
                    ops.extend(crate::harness::json::assign_json_ops(
                        wire::get(&value["generation"], "message"),
                        partial.value(),
                        &["generation".into(), "message".into()],
                    ));
                    live.apply_ops(&ops)?;
                    Ok(None)
                },
                &inner.context,
            ),
        };
        pi_chord::rt::spawn(async move {
            if let Err(error) = operation.await
                && !inner.runtime.signal().aborted()
            {
                let _ = inner.runtime.report(error);
            }
            inner.state.lock().in_flight = false;
            inner
                .changed
                .send_modify(|epoch| *epoch = epoch.wrapping_add(1));
            Self::schedule(&inner);
        });
    }
    async fn stop(&self) {
        let mut changed = self.0.changed.subscribe();
        let cancel = {
            let mut state = self.0.state.lock();
            state.stopped = true;
            state.pending = None;
            state.timer_generation = None;
            state.timer.take()
        };
        if let Some(cancel) = cancel {
            cancel();
        }
        loop {
            if !self.0.state.lock().in_flight {
                return;
            }
            if changed.changed().await.is_err() {
                return;
            }
        }
    }
}
impl Drop for PartialWriter {
    fn drop(&mut self) {
        let cancel = {
            let mut state = self.0.state.lock();
            state.stopped = true;
            state.pending = None;
            state.timer_generation = None;
            state.timer.take()
        };
        if let Some(cancel) = cancel {
            cancel();
        }
    }
}
