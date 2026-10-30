//! `context.*`: the native `pi_chord::context::Context` behind every JS `Context`
//! (`js/chord/context/index.js`). Values cross as handles, so `value(key)` returns the very JS
//! value stored; caller cancellation is mirrored into a native `AbortController` so a JS context
//! carries real native cancellation to Rust services.

use pi_chord::context::{
    self, AbortController, AbortError, AbortReason, BACKGROUND_CONTEXT, Context, ContextKey,
    TODO_CONTEXT,
};

use crate::errors::{HostError, HostResult};
use crate::host::{Args, Methods};
use crate::value::HostValue;

/// A JS context key: values are stored as handles (or plain host values).
pub type Key = ContextKey<HostValue>;

fn ctx(args: &Args, i: usize) -> HostResult<std::sync::Arc<Context>> {
    args.native::<Context>(i)
}

/// The native context behind a JS Chord context, as crossed by the glue's
/// `nativeContext(context)` (`pi-durable:internal/chord/context`); `undefined` is
/// `BACKGROUND_CONTEXT`.
pub fn context_from(value: &HostValue) -> HostResult<Context> {
    match value {
        HostValue::Undefined => Ok(BACKGROUND_CONTEXT.clone()),
        other => other
            .as_native::<Context>()
            .map(|context| context.as_ref().clone())
            .ok_or_else(|| {
                HostError::type_error(
                    "expected a native Chord context (pass nativeContext(context) from pi-durable:internal/chord/context)",
                )
            }),
    }
}

pub(super) fn register(m: &mut Methods) {
    m.sync("context.root", |_, args| {
        let name = args.text(0)?;
        Ok(HostValue::native(match name.as_str() {
            "BACKGROUND_CONTEXT" => BACKGROUND_CONTEXT.clone(),
            "TODO_CONTEXT" => TODO_CONTEXT.clone(),
            _ => {
                return Err(HostError::type_error(format!(
                    "unknown root context {name}"
                )));
            }
        }))
    });
    m.sync("context.key", |_, args| {
        let description = match args.get(0) {
            HostValue::Undefined => "anonymous".to_string(),
            _ => args.text(0)?,
        };
        Ok(HostValue::native(context::create_context_key::<HostValue>(
            description,
        )))
    });
    m.sync("context.withValue", |_, mut args| {
        let parent = ctx(&args, 0)?;
        let key = args.native::<Key>(1)?;
        let value = args.take(2);
        Ok(HostValue::native(context::with_context_value(
            &key, value, &parent,
        )))
    });
    m.sync("context.value", |_, args| {
        let context = ctx(&args, 0)?;
        let key = args.native::<Key>(1)?;
        // `[value]` when the chain binds the key (even to `undefined`), `undefined` when it does not.
        Ok(context
            .value(&key)
            .map(|value| HostValue::Array(vec![value.as_ref().clone()]))
            .unwrap_or(HostValue::Undefined))
    });
    m.sync("context.toString", |_, args| {
        Ok(HostValue::string(&ctx(&args, 0)?.to_string()))
    });
    m.sync("context.controller", |_, _| {
        Ok(HostValue::native(AbortController::new()))
    });
    m.sync("context.withAbortSignal", |_, args| {
        let parent = ctx(&args, 0)?;
        let controller = args.native::<AbortController>(1)?;
        Ok(HostValue::native(context::with_abort_signal(
            controller.signal(),
            &parent,
        )))
    });
    m.sync("context.withoutAbortSignal", |_, args| {
        Ok(HostValue::native(context::without_abort_signal(&*ctx(
            &args, 0,
        )?)))
    });
    m.sync("context.abort", |_, args| {
        let controller = args.native::<AbortController>(0)?;
        let message = match args.get(1) {
            HostValue::String(text) => text.to_string_lossy(),
            _ => "The operation was aborted".to_string(),
        };
        controller.abort_with_reason(AbortReason::Error(AbortError::new(message)));
        Ok(HostValue::Undefined)
    });
    m.sync("context.aborted", |_, args| {
        Ok(HostValue::Bool(ctx(&args, 0)?.check().is_err()))
    });
}
