//! Async event streams (port of `utils/event-stream.ts`).
//!
//! A stream is split into a consumer half ([`EventStream`], which implements
//! [`futures::Stream`] and exposes [`EventStream::result`]) and a cloneable producer
//! half ([`EventSender`]) with `push`/`end`, mirroring the TypeScript class.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use web_time::Instant;

use futures::Stream;
use parking_lot::Mutex;
use tokio::sync::{mpsc, watch};

use crate::types::{AssistantMessage, AssistantMessageEvent, StopReason};

type CompleteFn<T> = Arc<dyn Fn(&T) -> bool + Send + Sync>;
type ExtractFn<T, R> = Arc<dyn Fn(&T) -> R + Send + Sync>;
type TransformFn<T> = Arc<dyn Fn(&mut T) + Send + Sync>;

struct SenderInner<T, R> {
    tx: Option<mpsc::UnboundedSender<T>>,
    done: bool,
    result_tx: watch::Sender<Option<R>>,
    is_complete: CompleteFn<T>,
    extract_result: ExtractFn<T, R>,
    /// Hook applied to events before delivery while the stream is not done.
    before_push: Option<TransformFn<T>>,
    /// Hook applied to `end()` results while the stream is not done.
    before_end: Option<TransformFn<R>>,
}

/// Producer half of an [`EventStream`].
pub struct EventSender<T, R> {
    inner: Arc<Mutex<SenderInner<T, R>>>,
}

impl<T, R> Clone for EventSender<T, R> {
    fn clone(&self) -> Self {
        Self { inner: self.inner.clone() }
    }
}

impl<T, R: Clone> EventSender<T, R> {
    /// Push an event. Ignored once the stream is done.
    pub fn push(&self, mut event: T) {
        let mut inner = self.inner.lock();
        if inner.done {
            return;
        }
        if let Some(hook) = inner.before_push.clone() {
            hook(&mut event);
        }
        let complete = (inner.is_complete)(&event);
        if complete {
            inner.done = true;
            let result = (inner.extract_result)(&event);
            inner.result_tx.send_replace(Some(result));
        }
        if let Some(tx) = &inner.tx {
            let _ = tx.send(event);
        }
        if complete {
            inner.tx = None;
        }
    }

    /// End the stream, optionally resolving the final result.
    pub fn end(&self, result: Option<R>) {
        let mut inner = self.inner.lock();
        let was_done = inner.done;
        inner.done = true;
        if let Some(mut result) = result {
            if !was_done
                && let Some(hook) = inner.before_end.clone() {
                    hook(&mut result);
                }
            inner.result_tx.send_replace(Some(result));
        }
        inner.tx = None;
    }

    pub fn is_done(&self) -> bool {
        self.inner.lock().done
    }
}

/// Consumer half: an async stream of events with a final result.
pub struct EventStream<T, R> {
    rx: mpsc::UnboundedReceiver<T>,
    result_rx: watch::Receiver<Option<R>>,
}

impl<T, R: Clone> EventStream<T, R> {
    /// Create a stream. `is_complete` marks the terminal event; `extract_result` maps it to the result.
    pub fn new(
        is_complete: impl Fn(&T) -> bool + Send + Sync + 'static,
        extract_result: impl Fn(&T) -> R + Send + Sync + 'static,
    ) -> (EventSender<T, R>, EventStream<T, R>) {
        Self::with_hooks(Arc::new(is_complete), Arc::new(extract_result), None, None)
    }

    fn with_hooks(
        is_complete: CompleteFn<T>,
        extract_result: ExtractFn<T, R>,
        before_push: Option<TransformFn<T>>,
        before_end: Option<TransformFn<R>>,
    ) -> (EventSender<T, R>, EventStream<T, R>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let (result_tx, result_rx) = watch::channel(None);
        let sender = EventSender {
            inner: Arc::new(Mutex::new(SenderInner {
                tx: Some(tx),
                done: false,
                result_tx,
                is_complete,
                extract_result,
                before_push,
                before_end,
            })),
        };
        (sender, EventStream { rx, result_rx })
    }

    /// Receive the next event; `None` once the stream has ended and drained.
    pub async fn next_event(&mut self) -> Option<T> {
        self.rx.recv().await
    }

    /// Wait for the final result. `None` when every producer was dropped without one.
    pub async fn try_result(&self) -> Option<R> {
        let mut rx = self.result_rx.clone();
        loop {
            if let Some(value) = rx.borrow_and_update().clone() {
                return Some(value);
            }
            if rx.changed().await.is_err() {
                return rx.borrow().clone();
            }
        }
    }
}

impl<T, R> Stream for EventStream<T, R> {
    type Item = T;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        self.rx.poll_recv(cx)
    }
}

impl<T, R> Unpin for EventStream<T, R> {}

// ---------------------------------------------------------------------------
// Assistant message streams
// ---------------------------------------------------------------------------

/// Producer half of an [`AssistantMessageEventStream`].
pub type AssistantMessageEventSender = EventSender<AssistantMessageEvent, AssistantMessage>;

/// Event stream of one assistant response.
///
/// It also times the response: the final message (`done` or `error` event, or the
/// result passed to `end()`) gets `duration_ms`, measured with a monotonic clock from
/// the stream's creation, unless the message already has one or its `timestamp`
/// predates the stream.
pub struct AssistantMessageEventStream {
    inner: EventStream<AssistantMessageEvent, AssistantMessage>,
}

struct StreamTimer {
    started_at: i64,
    started_at_monotonic: Instant,
}

impl StreamTimer {
    fn time(&self, message: &mut AssistantMessage) {
        if message.duration_ms.is_some() || message.timestamp < self.started_at {
            return;
        }
        message.duration_ms = Some(self.started_at_monotonic.elapsed().as_millis() as u64);
    }
}

impl AssistantMessageEventStream {
    /// Create a new stream and its sender.
    pub fn new() -> (AssistantMessageEventSender, AssistantMessageEventStream) {
        let timer = Arc::new(StreamTimer {
            started_at: crate::utils::now_ms(),
            started_at_monotonic: Instant::now(),
        });
        let push_timer = timer.clone();
        let hook: TransformFn<AssistantMessageEvent> = Arc::new(move |event| {
            match event {
                AssistantMessageEvent::Done { message, .. } => push_timer.time(message),
                AssistantMessageEvent::Error { error, .. } => push_timer.time(error),
                _ => {}
            }
        });
        let end_hook: TransformFn<AssistantMessage> = Arc::new(move |message| timer.time(message));
        let (sender, stream) = EventStream::with_hooks(
            Arc::new(|event: &AssistantMessageEvent| event.is_terminal()),
            Arc::new(|event: &AssistantMessageEvent| match event {
                AssistantMessageEvent::Done { message, .. } => message.clone(),
                AssistantMessageEvent::Error { error, .. } => error.clone(),
                _ => unreachable!("only terminal events complete the stream"),
            }),
            Some(hook),
            Some(end_hook),
        );
        (sender, AssistantMessageEventStream { inner: stream })
    }

    /// Receive the next event; `None` once the stream has ended and drained.
    pub async fn next_event(&mut self) -> Option<AssistantMessageEvent> {
        self.inner.next_event().await
    }

    /// Wait for the final assistant message. When every producer was dropped without a
    /// result, returns an error message.
    pub async fn result(&self) -> AssistantMessage {
        match self.inner.try_result().await {
            Some(message) => message,
            None => AssistantMessage {
                stop_reason: StopReason::Error,
                error_message: Some("Stream ended without a result".to_string()),
                timestamp: crate::utils::now_ms(),
                ..Default::default()
            },
        }
    }

    /// Wait for the final result, `None` if producers dropped without one.
    pub async fn try_result(&self) -> Option<AssistantMessage> {
        self.inner.try_result().await
    }

    /// Drain all events and return them with the final result.
    pub async fn collect_all(mut self) -> (Vec<AssistantMessageEvent>, AssistantMessage) {
        let mut events = Vec::new();
        while let Some(event) = self.next_event().await {
            events.push(event);
        }
        let result = self.result().await;
        (events, result)
    }
}

impl Stream for AssistantMessageEventStream {
    type Item = AssistantMessageEvent;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

/// Factory function (for use in extensions).
pub fn create_assistant_message_event_stream() -> (AssistantMessageEventSender, AssistantMessageEventStream) {
    AssistantMessageEventStream::new()
}
