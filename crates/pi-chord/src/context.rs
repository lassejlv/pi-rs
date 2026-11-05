//! Explicit immutable invocation values, telemetry parents and cancellation.

use crate::JsonValue;
use futures::future::select_all;
use parking_lot::Mutex;
use pi_telemetry::{ContextRef, SpanOptions, TelemetryContextExt, noop_telemetry_context};
use std::{
    any::Any,
    fmt,
    future::Future,
    marker::PhantomData,
    sync::{
        Arc, LazyLock, Weak,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio_util::sync::CancellationToken;

static NEXT_KEY: AtomicU64 = AtomicU64::new(1);
static NEXT_ABORT: AtomicU64 = AtomicU64::new(1);

pub struct ContextKey<T> {
    id: u64,
    description: Arc<str>,
    marker: PhantomData<fn(T) -> T>,
}
impl<T> Clone for ContextKey<T> {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            description: self.description.clone(),
            marker: PhantomData,
        }
    }
}
pub fn create_context_key<T>(description: impl Into<Arc<str>>) -> ContextKey<T> {
    ContextKey {
        id: NEXT_KEY.fetch_add(1, Ordering::Relaxed),
        description: description.into(),
        marker: PhantomData,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct AbortError {
    pub message: String,
}
impl AbortError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum AbortReason {
    Error(AbortError),
    Value(JsonValue),
}
impl AbortReason {
    pub fn waiter_error(&self) -> AbortError {
        match self {
            Self::Error(error) => error.clone(),
            Self::Value(_) => AbortError::new("The operation was aborted"),
        }
    }
}

struct Source {
    token: CancellationToken,
    reason: Mutex<Option<(u64, AbortReason)>>,
}

#[derive(Clone)]
pub struct AbortSignal {
    sources: Arc<[Arc<Source>]>,
}

impl AbortSignal {
    pub fn any(signals: impl IntoIterator<Item = Self>) -> Self {
        let mut sources = Vec::new();
        for signal in signals {
            for source in signal.sources.iter() {
                if !sources.iter().any(|s| Arc::ptr_eq(s, source)) {
                    sources.push(source.clone());
                }
            }
        }
        Self {
            sources: sources.into(),
        }
    }
    pub fn aborted(&self) -> bool {
        self.sources.iter().any(|s| s.reason.lock().is_some())
    }
    pub fn reason(&self) -> Option<AbortReason> {
        self.sources
            .iter()
            .filter_map(|s| s.reason.lock().clone())
            .min_by_key(|(order, _)| *order)
            .map(|(_, reason)| reason)
    }
    pub async fn cancelled(&self) {
        if self.aborted() {
            return;
        }
        if self.sources.is_empty() {
            std::future::pending::<()>().await;
        }
        select_all(self.sources.iter().map(|s| Box::pin(s.token.cancelled()))).await;
    }
    pub fn check(&self) -> Result<(), AbortError> {
        match self.reason() {
            Some(reason) => Err(reason.waiter_error()),
            None => Ok(()),
        }
    }
    /// A `WeakRef` to this signal: it upgrades while any clone of it is still held.
    pub fn downgrade(&self) -> WeakAbortSignal {
        WeakAbortSignal {
            sources: Arc::downgrade(&self.sources),
        }
    }
}

/// Observes whether a signal is still retained without retaining it.
#[derive(Clone)]
pub struct WeakAbortSignal {
    sources: Weak<[Arc<Source>]>,
}
impl WeakAbortSignal {
    pub fn upgrade(&self) -> Option<AbortSignal> {
        self.sources
            .upgrade()
            .map(|sources| AbortSignal { sources })
    }
}
impl fmt::Debug for AbortSignal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AbortSignal")
            .field("reason", &self.reason())
            .finish()
    }
}

#[derive(Clone)]
pub struct AbortController {
    source: Arc<Source>,
}
impl Default for AbortController {
    fn default() -> Self {
        Self {
            source: Arc::new(Source {
                token: CancellationToken::new(),
                reason: Mutex::new(None),
            }),
        }
    }
}
impl AbortController {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn signal(&self) -> AbortSignal {
        AbortSignal {
            sources: vec![self.source.clone()].into(),
        }
    }
    pub fn abort(&self) {
        self.abort_with_reason(AbortReason::Error(AbortError::new(
            "This operation was aborted",
        )));
    }
    pub fn abort_with_reason(&self, reason: AbortReason) {
        let mut stored = self.source.reason.lock();
        if stored.is_some() {
            return;
        }
        *stored = Some((NEXT_ABORT.fetch_add(1, Ordering::SeqCst), reason));
        self.source.token.cancel();
    }
}

enum Node {
    Root(&'static str),
    Value {
        parent: Arc<Node>,
        id: u64,
        description: Arc<str>,
        value: Arc<dyn Any + Send + Sync>,
    },
}
impl fmt::Display for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Root(name) => write!(f, "[Context {name}]"),
            Self::Value {
                parent,
                description,
                ..
            } => write!(f, "{parent}.WithValue({description})"),
        }
    }
}

#[derive(Clone)]
pub struct Context {
    node: Arc<Node>,
    signal: Option<AbortSignal>,
    telemetry: ContextRef,
}
pub static BACKGROUND_CONTEXT: LazyLock<Context> =
    LazyLock::new(|| Context::root("BACKGROUND_CONTEXT"));
pub static TODO_CONTEXT: LazyLock<Context> = LazyLock::new(|| Context::root("TODO_CONTEXT"));
static SIGNAL_KEY: LazyLock<ContextKey<Option<AbortSignal>>> =
    LazyLock::new(|| create_context_key("chord.abortSignal"));
static TELEMETRY_KEY: LazyLock<ContextKey<ContextRef>> =
    LazyLock::new(|| create_context_key("chord.telemetry"));

impl Default for Context {
    fn default() -> Self {
        BACKGROUND_CONTEXT.clone()
    }
}
impl fmt::Display for Context {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.node.fmt(f)
    }
}
impl fmt::Debug for Context {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl Context {
    fn root(name: &'static str) -> Self {
        Self {
            node: Arc::new(Node::Root(name)),
            signal: None,
            telemetry: noop_telemetry_context(),
        }
    }
    pub fn value<T: Any + Send + Sync>(&self, key: &ContextKey<T>) -> Option<Arc<T>> {
        let mut node = self.node.as_ref();
        loop {
            match node {
                Node::Root(_) => return None,
                Node::Value {
                    parent, id, value, ..
                } => {
                    if *id == key.id {
                        return value.clone().downcast().ok();
                    }
                    node = parent.as_ref();
                }
            }
        }
    }
    pub fn abort_signal(&self) -> Option<&AbortSignal> {
        self.signal.as_ref()
    }
    pub fn telemetry(&self) -> &ContextRef {
        &self.telemetry
    }
    pub fn check(&self) -> Result<(), AbortError> {
        self.signal.as_ref().map_or(Ok(()), AbortSignal::check)
    }
}

pub fn with_context_value<T: Any + Send + Sync>(
    key: &ContextKey<T>,
    value: T,
    parent: &Context,
) -> Context {
    Context {
        node: Arc::new(Node::Value {
            parent: parent.node.clone(),
            id: key.id,
            description: key.description.clone(),
            value: Arc::new(value),
        }),
        signal: parent.signal.clone(),
        telemetry: parent.telemetry.clone(),
    }
}
pub fn with_telemetry(telemetry: ContextRef, context: &Context) -> Context {
    let mut derived = with_context_value(&TELEMETRY_KEY, telemetry.clone(), context);
    derived.telemetry = telemetry;
    derived
}
pub fn with_abort_signal(signal: AbortSignal, context: &Context) -> Context {
    let signal = match &context.signal {
        Some(parent) => AbortSignal::any([parent.clone(), signal]),
        None => signal,
    };
    let mut derived = with_context_value(&SIGNAL_KEY, Some(signal.clone()), context);
    derived.signal = Some(signal);
    derived
}
pub fn without_abort_signal(context: &Context) -> Context {
    let mut derived = with_context_value(&SIGNAL_KEY, None, context);
    derived.signal = None;
    derived
}
pub fn with_cancel(context: &Context) -> (Context, AbortController) {
    let controller = AbortController::new();
    (with_abort_signal(controller.signal(), context), controller)
}

pub async fn trace_in<F, Fut>(
    options: impl Into<SpanOptions>,
    callback: F,
    context: &Context,
) -> Fut::Output
where
    F: FnOnce(Context) -> Fut,
    Fut: Future,
{
    context
        .telemetry
        .start_span(options, |span| callback(with_telemetry(span, context)))
        .await
}

#[derive(Debug, thiserror::Error)]
pub enum AwaitError {
    #[error(transparent)]
    Aborted(#[from] AbortError),
    #[error(transparent)]
    Task(#[from] crate::rt::JoinError),
}

/// Run an operation independently and cancel only this waiter. Dropping the
/// waiter's join handle leaves the underlying operation running, as a JS promise does.
pub async fn await_with_context<F, T>(operation: F, context: &Context) -> Result<T, AwaitError>
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let handle = crate::rt::spawn(operation);
    let Some(signal) = &context.signal else {
        return Ok(handle.await?);
    };
    signal.check()?;
    tokio::select! {
        biased;
        _ = signal.cancelled() => Err(signal.reason().expect("cancelled signal has a reason").waiter_error().into()),
        result = handle => Ok(result?),
    }
}
