//! Typed registration helpers. Values and callback owners pass through unchanged.
use super::{
    prompt::{NativeSection, NativeSectionRender},
    registry::{Extension, HookRegistration, RegisteredSection, RegisteredTool, Wrap},
};
use crate::{JsonString, Result, Task};
use std::sync::Arc;

pub fn define_extension<T: RegisteredTool, S: RegisteredSection>(
    extension: Extension<T, S>,
) -> Extension<T, S> {
    extension
}

pub fn define_tool<T: RegisteredTool>(tool: T) -> T {
    tool
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SectionOptions {
    pub tag: Option<bool>,
}

pub fn section(
    key: impl Into<JsonString>,
    render: NativeSectionRender,
    options: SectionOptions,
) -> NativeSection {
    NativeSection {
        key: key.into(),
        tagged: options.tag.unwrap_or(true),
        render,
    }
}

pub fn hook<I, S, R, H: Send + Sync + 'static>(
    task: &Task<I, S, R, H>,
    handlers: Arc<H>,
) -> HookRegistration {
    HookRegistration::for_task(task, handlers)
}

pub fn wrap_tool<T: RegisteredTool, S>(
    tool: &T,
    wrapper: impl Fn(Arc<T>) -> Result<Arc<T>> + Send + Sync + 'static,
) -> Wrap<T, S> {
    Wrap::Tool {
        name: tool.name().clone(),
        apply: Arc::new(wrapper),
    }
}

pub fn wrap_section<T, S: RegisteredSection>(
    key: impl Into<JsonString>,
    wrapper: impl Fn(Arc<S>) -> Result<Arc<S>> + Send + Sync + 'static,
) -> Wrap<T, S> {
    Wrap::Section {
        key: key.into(),
        apply: Arc::new(wrapper),
    }
}
