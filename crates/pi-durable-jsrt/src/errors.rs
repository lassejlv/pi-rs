//! Typed errors crossing the host boundary.
//!
//! A Rust method fails with a [`HostError`]. A [`HostError::Typed`] names an [`ErrorClass`]; on
//! the JS side `js/bridge.js` (`__piDurable.makeError`) reconstructs an instance of the matching
//! upstream class, so `instanceof ConversationBusy` and friends hold. A [`HostError::Thrown`] is
//! a JS value thrown by a JS callback, carried by identity: a callback's error rethrows as the
//! very same object.
//!
//! Class table (discriminator → JS class, constructor inputs taken from `props`):
//!
//! | `ErrorClass` | JS class (module) | props |
//! | --- | --- | --- |
//! | `Error`, `TypeError`, `RangeError` | builtins | any |
//! | `AbortError` | `DOMException(message, "AbortError")` | |
//! | `ConversationBusy` | `@earendil-works/pi-durable` | `conversationId` |
//! | `ReadAfterWrite` | `@earendil-works/pi-durable` | `method` |
//! | `ReadOnlySession` | `@earendil-works/pi-durable` | `what` |
//! | `SessionFailed` | `@earendil-works/pi-durable` | cause |
//! | `StorageRequestError` | `@earendil-works/pi-durable` | message |
//! | `FileError` | `@earendil-works/pi-durable/env` | `code`, `path`, cause |
//! | `ExecutionError` | `@earendil-works/pi-durable/env` | `code`, `spillPath`, cause |
//! | `JsonlCorruptionError` | `@earendil-works/pi-durable/storage/jsonl` | message, cause |
//! | `JsonlStoragePoisonedError` | `@earendil-works/pi-durable/storage/jsonl` | cause |
//! | `ChordError` | `@earendil-works/chord` | `code`, `reason`, `details`, cause (message without the `code/reason: ` prefix) |
//! | `PathError` | `@earendil-works/chord/delta` | `path` |
//! | `UnsafePathError` | `@earendil-works/chord/delta` | `segment` |
//! | `WatchError` | `Error` named `WatchError` (no upstream class) | |
//!
//! Classes whose upstream constructor builds the message (e.g. `ConversationBusy`) ignore the
//! native message; the others use it. Props the constructor does not consume are assigned to
//! the instance.

use std::sync::Arc;

use pi_jsrt::js::{Context, JsError, JsObject, JsValue, js_string};
use pi_json::strict::JsonString;
use pi_jsrt::JsHandle;

use crate::value::{self, HostValue};

/// Error classes the host can raise (see the module table).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[allow(missing_docs)]
pub enum ErrorClass {
    Error,
    TypeError,
    RangeError,
    AbortError,
    ConversationBusy,
    ReadAfterWrite,
    ReadOnlySession,
    SessionFailed,
    StorageRequestError,
    FileError,
    ExecutionError,
    JsonlCorruptionError,
    JsonlStoragePoisonedError,
    ChordError,
    PathError,
    UnsafePathError,
    WatchError,
}

impl ErrorClass {
    /// Every class, in table order.
    pub const ALL: &'static [ErrorClass] = &[
        ErrorClass::Error,
        ErrorClass::TypeError,
        ErrorClass::RangeError,
        ErrorClass::AbortError,
        ErrorClass::ConversationBusy,
        ErrorClass::ReadAfterWrite,
        ErrorClass::ReadOnlySession,
        ErrorClass::SessionFailed,
        ErrorClass::StorageRequestError,
        ErrorClass::FileError,
        ErrorClass::ExecutionError,
        ErrorClass::JsonlCorruptionError,
        ErrorClass::JsonlStoragePoisonedError,
        ErrorClass::ChordError,
        ErrorClass::PathError,
        ErrorClass::UnsafePathError,
        ErrorClass::WatchError,
    ];

    /// The discriminator, which is also the JS `name` of the reconstructed error.
    pub fn name(self) -> &'static str {
        match self {
            ErrorClass::Error => "Error",
            ErrorClass::TypeError => "TypeError",
            ErrorClass::RangeError => "RangeError",
            ErrorClass::AbortError => "AbortError",
            ErrorClass::ConversationBusy => "ConversationBusy",
            ErrorClass::ReadAfterWrite => "ReadAfterWrite",
            ErrorClass::ReadOnlySession => "ReadOnlySession",
            ErrorClass::SessionFailed => "SessionFailed",
            ErrorClass::StorageRequestError => "StorageRequestError",
            ErrorClass::FileError => "FileError",
            ErrorClass::ExecutionError => "ExecutionError",
            ErrorClass::JsonlCorruptionError => "JsonlCorruptionError",
            ErrorClass::JsonlStoragePoisonedError => "JsonlStoragePoisonedError",
            ErrorClass::ChordError => "ChordError",
            ErrorClass::PathError => "PathError",
            ErrorClass::UnsafePathError => "UnsafePathError",
            ErrorClass::WatchError => "WatchError",
        }
    }

    /// The class for a discriminator.
    pub fn from_name(name: &str) -> Option<ErrorClass> {
        ErrorClass::ALL.iter().copied().find(|c| c.name() == name)
    }

    /// The embedded module exporting the JS class (`None` for builtins).
    pub fn module(self) -> Option<&'static str> {
        match self {
            ErrorClass::Error
            | ErrorClass::TypeError
            | ErrorClass::RangeError
            | ErrorClass::AbortError
            | ErrorClass::WatchError => None,
            ErrorClass::ConversationBusy
            | ErrorClass::ReadAfterWrite
            | ErrorClass::ReadOnlySession
            | ErrorClass::SessionFailed
            | ErrorClass::StorageRequestError => Some("@earendil-works/pi-durable"),
            ErrorClass::FileError | ErrorClass::ExecutionError => {
                Some("@earendil-works/pi-durable/env")
            }
            ErrorClass::JsonlCorruptionError | ErrorClass::JsonlStoragePoisonedError => {
                Some("@earendil-works/pi-durable/storage/jsonl")
            }
            ErrorClass::ChordError => Some("@earendil-works/chord"),
            ErrorClass::PathError | ErrorClass::UnsafePathError => {
                Some("@earendil-works/chord/delta")
            }
        }
    }
}

/// A typed host error (see the module docs).
#[derive(Clone, Debug)]
pub struct TypedError {
    /// Which class to reconstruct.
    pub class: ErrorClass,
    /// The message (UTF-16, lossless).
    pub message: JsonString,
    /// Constructor inputs and extra properties.
    pub props: Vec<(String, HostValue)>,
    /// `error.cause`.
    pub cause: Option<HostError>,
}

/// A value thrown by JS, kept by identity.
#[derive(Clone, Debug)]
pub struct Thrown {
    /// The thrown value.
    pub value: JsHandle,
    /// Its `name` when it is an object with a string name (`"Error"` otherwise).
    pub name: String,
    /// Its `message` (or `String(value)` for primitives).
    pub message: JsonString,
}

/// An error crossing the host boundary.
#[derive(Clone, Debug)]
pub enum HostError {
    /// Raised by Rust; reconstructed as an instance of its [`ErrorClass`].
    Typed(Arc<TypedError>),
    /// Thrown by JS; rethrown as the same value.
    Thrown(Arc<Thrown>),
}

/// Result of a host method.
pub type HostResult<T> = Result<T, HostError>;

impl HostError {
    /// A typed error of `class` with `message`.
    pub fn new(class: ErrorClass, message: impl Into<String>) -> HostError {
        HostError::typed(class, JsonString::from(message.into()))
    }

    /// A typed error with a UTF-16 message.
    pub fn typed(class: ErrorClass, message: JsonString) -> HostError {
        HostError::Typed(Arc::new(TypedError {
            class,
            message,
            props: Vec::new(),
            cause: None,
        }))
    }

    /// A `TypeError`.
    pub fn type_error(message: impl Into<String>) -> HostError {
        HostError::new(ErrorClass::TypeError, message)
    }

    /// Add a property (constructor input or extra member). No effect on thrown values.
    pub fn with_prop(mut self, key: &str, value: impl Into<HostValue>) -> HostError {
        if let HostError::Typed(t) = &mut self {
            Arc::make_mut(t).props.push((key.to_string(), value.into()));
        }
        self
    }

    /// Set `error.cause`. No effect on thrown values.
    pub fn with_cause(mut self, cause: HostError) -> HostError {
        if let HostError::Typed(t) = &mut self {
            Arc::make_mut(t).cause = Some(cause);
        }
        self
    }

    /// The class of a typed error (`None` for a thrown JS value).
    pub fn class(&self) -> Option<ErrorClass> {
        match self {
            HostError::Typed(t) => Some(t.class),
            HostError::Thrown(_) => None,
        }
    }

    /// The JS `name` of the error.
    pub fn name(&self) -> &str {
        match self {
            HostError::Typed(t) => t.class.name(),
            HostError::Thrown(t) => &t.name,
        }
    }

    /// The message.
    pub fn message(&self) -> &JsonString {
        match self {
            HostError::Typed(t) => &t.message,
            HostError::Thrown(t) => &t.message,
        }
    }

    /// Wrap a thrown JS value (reads its `name` and `message`).
    pub fn from_js(ctx: &mut Context, thrown: &JsValue) -> HostError {
        let (name, message) = describe(ctx, thrown);
        match pi_jsrt::ctx::make_handle(ctx, thrown.clone()) {
            Some(value) => HostError::Thrown(Arc::new(Thrown {
                value,
                name,
                message,
            })),
            None => HostError::typed(ErrorClass::Error, message),
        }
    }

    /// Wrap a JS error (a thrown value or a native error not yet thrown).
    pub fn from_error(ctx: &mut Context, error: JsError) -> HostError {
        match error.into_opaque(ctx) {
            Ok(value) => HostError::from_js(ctx, &value),
            Err(e) => HostError::new(ErrorClass::Error, e.to_string()),
        }
    }

    /// The JS value to throw for this error.
    pub fn to_js(&self, ctx: &mut Context) -> JsValue {
        match self {
            HostError::Thrown(t) => pi_jsrt::ctx::handle_value(ctx, &t.value).unwrap_or_default(),
            HostError::Typed(t) => typed_to_js(ctx, t),
        }
    }

    /// The [`JsError`] throwing [`HostError::to_js`].
    pub fn to_error(&self, ctx: &mut Context) -> JsError {
        JsError::from_opaque(self.to_js(ctx))
    }
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.name(), self.message())
    }
}

impl std::error::Error for HostError {}

fn describe(ctx: &mut Context, value: &JsValue) -> (String, JsonString) {
    let Some(obj) = value.as_object() else {
        let text = value
            .to_string(ctx)
            .map(|s| value::js_string_units(&s))
            .unwrap_or_default();
        return ("Error".to_string(), text);
    };
    let name = obj
        .get(js_string!("name"), ctx)
        .ok()
        .and_then(|v| v.as_string().map(|s| s.to_std_string_lossy()))
        .unwrap_or_else(|| "Error".to_string());
    let message = obj
        .get(js_string!("message"), ctx)
        .ok()
        .and_then(|v| v.as_string().map(|s| value::js_string_units(&s)))
        .unwrap_or_default();
    (name, message)
}

fn typed_to_js(ctx: &mut Context, t: &TypedError) -> JsValue {
    let props = JsObject::with_object_proto(ctx.intrinsics());
    for (k, v) in &t.props {
        if let Ok(v) = value::to_js(ctx, v) {
            let _ =
                props.create_data_property_or_throw(pi_jsrt::js::JsString::from(k.as_str()), v, ctx);
        }
    }
    let message: JsValue = value::units_js_string(&t.message).into();
    let cause = t.cause.as_ref().map(|c| c.to_js(ctx));
    let factory = ctx
        .global_object()
        .get(js_string!("__piDurable"), ctx)
        .ok()
        .and_then(|raw| raw.as_object())
        .and_then(|raw| raw.get(js_string!("makeError"), ctx).ok())
        .and_then(|f| f.as_callable());
    if let Some(factory) = factory {
        let args = [
            pi_jsrt::js::JsString::from(t.class.name()).into(),
            message.clone(),
            props.clone().into(),
            cause.clone().unwrap_or_default(),
            JsValue::from(cause.is_some()),
        ];
        if let Ok(v) = factory.call(&JsValue::undefined(), &args, ctx) {
            return v;
        }
    }
    // The bridge is not loaded: a builtin error carrying the name and props.
    let native = match t.class {
        ErrorClass::TypeError => pi_jsrt::js::JsNativeError::typ(),
        ErrorClass::RangeError => pi_jsrt::js::JsNativeError::range(),
        _ => pi_jsrt::js::JsNativeError::error(),
    }
    .with_message(t.message.to_string_lossy());
    let value = JsError::from(native).into_opaque(ctx).unwrap_or_default();
    if let Some(obj) = value.as_object() {
        let _ = obj.set(
            js_string!("name"),
            pi_jsrt::js::JsString::from(t.class.name()),
            true,
            ctx,
        );
        if let Some(cause) = cause {
            let _ = obj.set(js_string!("cause"), cause, true, ctx);
        }
        for (k, v) in &t.props {
            if let Ok(v) = value::to_js(ctx, v) {
                let _ = obj.set(pi_jsrt::js::JsString::from(k.as_str()), v, true, ctx);
            }
        }
    }
    value
}
