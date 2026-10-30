//! Helpers for code running on the JS thread via [`crate::JsRuntime::with_context`].
//!
//! These let embedders build host objects directly with the QuickJS API ([`crate::js`]) while still
//! interoperating with handles and JSON conversion.

use std::future::Future;

use crate::js::{Context, JsResult, JsValue};
use serde_json::Value;

use crate::engine::state;
use crate::error::Result;
use crate::runtime::{JsHandle, JsRuntime};

/// The runtime owning this context (if it is still alive).
pub fn runtime(ctx: &Context) -> Option<JsRuntime> {
    state(ctx).rt.upgrade().map(|inner| JsRuntime { inner })
}

/// Convert a JS value to JSON (functions become handle markers).
pub fn to_json(ctx: &mut Context, value: &JsValue) -> Result<Value> {
    crate::convert::to_json(value, ctx)
}

/// Convert JSON to a JS value (handle / host-function markers are revived).
pub fn from_json(ctx: &mut Context, value: &Value) -> JsResult<JsValue> {
    crate::convert::from_json(value, ctx)
}

/// Keep `value` alive in the handle table and return an owned handle to it.
pub fn make_handle(ctx: &mut Context, value: JsValue) -> Option<JsHandle> {
    let rt = runtime(ctx)?;
    let id = state(ctx).new_handle(value);
    rt.handle(&serde_json::json!({ crate::convert::HANDLE_KEY: id }))
}

/// Look up the JS value behind a handle.
pub fn handle_value(ctx: &Context, handle: &JsHandle) -> Option<JsValue> {
    state(ctx).get_handle(handle.id())
}

/// Number of live entries in the handle table.
pub fn handle_count(ctx: &Context) -> usize {
    state(ctx).handle_count()
}

/// Run all pending promise jobs now.
pub fn run_jobs(ctx: &mut Context) {
    crate::engine::run_microtasks(ctx);
}

/// Run `f` as the body of a synchronous host function implemented directly with [`crate::js`]. While it
/// runs, blocking calls into the runtime from this thread ([`crate::JsHandle::call_blocking`],
/// [`crate::JsRuntime::with_context_blocking`], [`with_current`]) run re-entrantly on `ctx`
/// instead of failing with [`crate::Error::Deadlock`]. `f` must reach the context only through
/// [`with_current`].
pub fn host_call<R>(ctx: &mut Context, f: impl FnOnce() -> R) -> R {
    crate::runtime::enter_host_call(ctx, f)
}

/// Run `f` with the context of the host call executing on this thread (see [`host_call`] and
/// [`crate::JsRuntime::register_function`]); `None` outside a host call.
pub fn with_current<R>(f: impl FnOnce(&mut Context) -> R) -> Option<R> {
    crate::runtime::with_current_ctx(f)
}

/// Conversion run on the JS thread once a [`spawn_promise`] future completes.
pub type Settle = Box<dyn FnOnce(&mut Context) -> JsResult<JsValue> + Send>;

/// Spawn `fut` on the runtime's tokio handle and return a promise settled on the JS thread with
/// the value (or error) its [`Settle`] produces. The pending operation keeps the event loop
/// alive, like the runtime's own async ops.
pub fn spawn_promise<F>(ctx: &mut Context, fut: F) -> JsValue
where
    F: Future<Output = Settle> + Send + 'static,
{
    crate::engine::spawn_promise(ctx, fut)
}

/// Wait for `value` (awaiting it when it is a promise) and call `done` on the JS thread with the
/// fulfilled value (`Ok`) or the rejection reason / thrown value (`Err`).
pub fn when_settled(ctx: &mut Context, value: JsResult<JsValue>, done: impl FnOnce(&mut Context, std::result::Result<JsValue, JsValue>) + 'static) {
    crate::engine::when_settled(ctx, value, done)
}
